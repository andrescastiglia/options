use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    ensure_private_dir_with_hook(path, |_| Ok(()))
}

fn ensure_private_dir_with_hook(
    path: &Path,
    after_create: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    after_create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other(format!(
            "la ruta privada no es un directorio regular: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        // No modificar directorios compartidos ajenos (por ejemplo `/tmp`). Los
        // directorios de estado propios sí quedan privados, incluso si ya existían.
        if metadata.uid() == unsafe { libc::geteuid() } {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

pub fn open_private_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    private_options(&mut options);
    let file = options.open(path)?;
    validate_open_file(&file, path, u64::MAX, true)?;
    Ok(file)
}

pub fn open_private_append_bounded(path: &Path, max_bytes: u64) -> std::io::Result<File> {
    let file = open_private_append(path)?;
    if file.metadata()?.len() >= max_bytes {
        return Err(std::io::Error::other(format!(
            "archivo append-only alcanzó el límite de {max_bytes} bytes: {}",
            path.display()
        )));
    }
    Ok(file)
}

pub fn open_private_read_write(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    private_options(&mut options);
    let file = options.open(path)?;
    validate_open_file(&file, path, u64::MAX, true)?;
    Ok(file)
}

pub fn open_private_read(path: &Path, max_bytes: u64) -> std::io::Result<File> {
    validate_private_file(path, max_bytes)?;
    let mut options = OpenOptions::new();
    options.read(true);
    private_options(&mut options);
    let file = options.open(path)?;
    validate_open_file(&file, path, max_bytes, true)?;
    Ok(file)
}

pub fn open_limited_read(path: &Path, max_bytes: u64) -> std::io::Result<File> {
    validated_file_metadata(path, max_bytes)?;
    let mut options = OpenOptions::new();
    options.read(true);
    private_options(&mut options);
    let file = options.open(path)?;
    validate_open_file(&file, path, max_bytes, false)?;
    Ok(file)
}

fn validate_open_file(
    file: &File,
    path: &Path,
    max_bytes: u64,
    require_private: bool,
) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(std::io::Error::other(format!(
            "archivo inválido o mayor que {max_bytes} bytes durante la apertura: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    if require_private {
        validate_private_permissions(&metadata, path)?;
    }
    Ok(())
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_with_hook(path, bytes, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicWriteStage {
    TemporaryOpened,
    ContentsWritten,
    FileSynced,
    Renamed,
    DirectorySynced,
}

fn write_atomic_with_hook(
    path: &Path,
    bytes: &[u8],
    mut stage_hook: impl FnMut(AtomicWriteStage) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;
    reject_symlink(path)?;
    let temporary = unique_temporary_path(path);
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    private_options(&mut options);
    let mut file = options.open(&temporary)?;
    let result = (|| {
        stage_hook(AtomicWriteStage::TemporaryOpened)?;
        file.write_all(bytes)?;
        stage_hook(AtomicWriteStage::ContentsWritten)?;
        file.sync_all()?;
        stage_hook(AtomicWriteStage::FileSynced)?;
        std::fs::rename(&temporary, path)?;
        stage_hook(AtomicWriteStage::Renamed)?;
        File::open(parent)?.sync_all()?;
        stage_hook(AtomicWriteStage::DirectorySynced)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;
    reject_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    private_options(&mut options);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(parent)?.sync_all()
}

pub fn reject_symlink(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::other(format!(
            "se rechazó un enlace simbólico en una ruta sensible: {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn read_private_limited(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = open_private_read(path, max_bytes)?;
    read_open_file_limited(&mut file, max_bytes)
}

pub fn validate_private_file(path: &Path, max_bytes: u64) -> std::io::Result<std::fs::Metadata> {
    let metadata = validated_file_metadata(path, max_bytes)?;
    #[cfg(unix)]
    validate_private_permissions(&metadata, path)?;
    Ok(metadata)
}

pub fn read_limited(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut file = open_limited_read(path, max_bytes)?;
    read_open_file_limited(&mut file, max_bytes)
}

fn read_open_file_limited(file: &mut File, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::other(format!(
            "archivo creció por encima de {max_bytes} bytes durante la lectura"
        )));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &std::fs::Metadata, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "el archivo privado debe tener permisos 0600: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validated_file_metadata(path: &Path, max_bytes: u64) -> std::io::Result<std::fs::Metadata> {
    reject_symlink(path)?;
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(std::io::Error::other(format!(
            "archivo inválido o mayor que {max_bytes} bytes: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        sequence
    ))
}

fn private_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_contents_and_uses_private_permissions() {
        let directory = tempfile_dir("atomic");
        let path = directory.join("state.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sensitive_writes_reject_symlinks() {
        use std::os::unix::fs::symlink;
        let directory = tempfile_dir("symlink");
        let target = directory.join("target");
        std::fs::write(&target, b"safe").unwrap();
        let link = directory.join("state");
        symlink(&target, &link).unwrap();
        assert!(write_atomic(&link, b"unsafe").is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"safe");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bounded_append_fails_before_growing_an_exhausted_file() {
        let directory = tempfile_dir("bounded");
        let path = directory.join("telemetry.jsonl");
        let file = open_private_append(&path).unwrap();
        file.set_len(10).unwrap();
        drop(file);
        assert!(open_private_append_bounded(&path, 10).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 10);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_atomic_write_fault_boundary_preserves_a_complete_version() {
        let directory = tempfile_dir("atomic-faults");
        let path = directory.join("state.json");
        write_atomic(&path, b"old-complete-version").unwrap();

        for fault in [
            AtomicWriteStage::TemporaryOpened,
            AtomicWriteStage::ContentsWritten,
            AtomicWriteStage::FileSynced,
            AtomicWriteStage::Renamed,
            AtomicWriteStage::DirectorySynced,
        ] {
            write_atomic(&path, b"old-complete-version").unwrap();
            let result = write_atomic_with_hook(&path, b"new-complete-version", |stage| {
                if stage == fault {
                    Err(std::io::Error::other("falla durable inyectada"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            let contents = std::fs::read(&path).unwrap();
            assert!(
                contents == b"old-complete-version" || contents == b"new-complete-version",
                "la frontera {fault:?} dejó contenido parcial"
            );
            let leaked_temporary = std::fs::read_dir(&directory).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".state.json.tmp.")
            });
            assert!(!leaked_temporary);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn existing_private_file_with_broad_permissions_is_rejected_for_read_and_append() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile_dir("permissions");
        let path = directory.join("journal.jsonl");
        std::fs::write(&path, b"sensitive").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            open_private_read(&path, 1024).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            open_private_append(&path).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"sensitive");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn limited_reader_rejects_size_before_allocating_or_parsing() {
        let directory = tempfile_dir("limited-read");
        let path = directory.join("input.json");
        std::fs::write(&path, b"12345").unwrap();
        let error = read_limited(&path, 4).unwrap_err();
        assert!(error.to_string().contains("mayor que 4 bytes"));
        assert_eq!(read_limited(&path, 5).unwrap(), b"12345");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn private_validation_preserves_bytes_and_rejects_size_and_file_type() {
        let directory = tempfile_dir("private-read-contract");
        let path = directory.join("private.bin");
        write_atomic(&path, b"12345").unwrap();

        assert_eq!(read_private_limited(&path, 5).unwrap(), b"12345");
        assert!(validate_private_file(&path, 4).is_err());
        assert!(validate_private_file(&directory, u64::MAX).is_err());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_type_descriptor_and_path_error_boundaries_fail_closed() {
        use std::os::unix::fs::symlink;

        let directory = tempfile_dir("type-boundaries");
        let linked_directory = directory.with_file_name(format!(
            "{}-link",
            directory.file_name().unwrap().to_string_lossy()
        ));
        symlink(&directory, &linked_directory).unwrap();
        assert!(ensure_private_dir(&linked_directory).is_err());

        let path = directory.join("input.bin");
        std::fs::write(&path, b"12345").unwrap();
        let regular = File::open(&path).unwrap();
        assert!(validate_open_file(&regular, &path, 4, false).is_err());
        let opened_directory = File::open(&directory).unwrap();
        assert!(validate_open_file(&opened_directory, &directory, 1024, false).is_err());

        let mut growing = File::open(&path).unwrap();
        assert!(read_open_file_limited(&mut growing, 4).is_err());
        assert!(read_limited(&directory, 1024).is_err());
        assert_eq!(
            reject_symlink(Path::new("invalid\0path"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(open_private_append(Path::new("/")).is_err());
        assert!(open_private_read_write(Path::new("/")).is_err());

        std::fs::remove_file(linked_directory).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_substitution_between_creation_and_validation_is_rejected() {
        let directory = tempfile_dir("directory-substitution");
        let result = ensure_private_dir_with_hook(&directory, |path| {
            std::fs::remove_dir(path)?;
            std::fs::write(path, b"not-a-directory")
        });
        assert!(result.is_err());
        assert!(directory.is_file());
        std::fs::remove_file(directory).unwrap();
    }

    #[test]
    fn io_failures_propagate_before_sensitive_outputs_are_created() {
        let directory = tempfile_dir("io-errors");
        let blocker = directory.join("regular-file");
        std::fs::write(&blocker, b"blocks descendants").unwrap();
        let impossible = blocker.join("secret.json");

        assert!(ensure_private_dir(&impossible).is_err());
        assert!(open_private_append(&impossible).is_err());
        assert!(open_private_read_write(&impossible).is_err());
        assert!(write_atomic(&impossible, b"secret").is_err());
        assert!(write_new(&impossible, b"secret").is_err());
        assert!(open_private_read(&impossible, 1024).is_err());
        assert!(open_limited_read(&impossible, 1024).is_err());

        let vanished = directory.join("vanished");
        assert!(ensure_private_dir_with_hook(&vanished, |path| std::fs::remove_dir(path)).is_err());
        assert!(!vanished.exists());
        assert_eq!(std::fs::read(&blocker).unwrap(), b"blocks descendants");
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn tempfile_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "options-secure-fs-{label}-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }
}
