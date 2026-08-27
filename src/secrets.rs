use std::{env, path::Path};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    digest, hkdf, hmac,
    rand::{SecureRandom, SystemRandom},
};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::secure_fs::write_new;

const MACHINE_ID_PATH: &str = "/etc/machine-id";
const LEGACY_FORMAT_VERSION: u8 = 1;
const LEGACY_MASTER_FORMAT_VERSION: u8 = 2;
const MASTER_FORMAT_VERSION: u8 = 3;
const NONCE_LENGTH: usize = 12;
const MIN_NON_EMPTY_CIPHERTEXT_BYTES: usize = 30;
const MAX_KEY_FILE_BYTES: u64 = 4_096;
const MAX_MACHINE_ID_BYTES: u64 = 4_096;
const LEGACY_KEY_CONTEXT: &[u8] = b"options-trading:iol-password:v1\0";
const PASSWORD_KEY_CONTEXT: &[u8] = b"options-trading:iol-password:v3\0";
const HKDF_SALT: &[u8] = b"options-trading:master-key:hkdf-sha256:v1\0";
const AUTH_KEY_CONTEXT: &[u8] = b"options-trading:live-authorization:v2\0";
const DATASET_KEY_CONTEXT: &[u8] = b"options-trading:dataset-manifest:v1\0";
const JOURNAL_KEY_CONTEXT: &[u8] = b"options-trading:journal:v1\0";
const RELEASE_READINESS_KEY_CONTEXT: &[u8] = b"options-trading:release-readiness:v2\0";
pub const MASTER_KEY_ENV: &str = "OPTIONS_MASTER_KEY_PATH";

#[cfg(test)]
pub(crate) static MASTER_KEY_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("no se pudo leer {MACHINE_ID_PATH}: {0}")]
    MachineId(std::io::Error),
    #[error("{MACHINE_ID_PATH} está vacío")]
    EmptyMachineId,
    #[error("no se pudo generar un valor aleatorio seguro")]
    Random,
    #[error("el secreto no puede estar vacío")]
    EmptySecret,
    #[error("el texto cifrado no es un Base64 válido")]
    InvalidBase64,
    #[error("el texto cifrado está incompleto o pertenece a otra versión")]
    InvalidFormat,
    #[error("no se pudo descifrar; comprobar que el valor fue creado en esta máquina")]
    Decryption,
    #[error("la contraseña descifrada no es texto válido")]
    InvalidText,
    #[error(
        "falta {MASTER_KEY_ENV}; debe apuntar a una clave privada creada con --init-master-key"
    )]
    MasterKeyMissing,
    #[error("no se pudo leer la clave maestra: {0}")]
    MasterKeyIo(std::io::Error),
    #[error("la clave maestra debe ser Base64 de exactamente 32 bytes")]
    InvalidMasterKey,
    #[error("la clave maestra permite acceso de grupo/otros; debe usar modo 0600")]
    InsecureMasterKeyPermissions,
}

pub fn encrypt_for_this_machine(plaintext: &str) -> Result<String, SecretError> {
    let master = master_key()?;
    let key = derive_hkdf_context_key(&master, PASSWORD_KEY_CONTEXT)?;
    encrypt_with_key(plaintext, &key, MASTER_FORMAT_VERSION).map(|encoded| format!("v3:{encoded}"))
}

fn encrypt_with_key(
    plaintext: &str,
    key: &[u8; 32],
    format_version: u8,
) -> Result<String, SecretError> {
    if plaintext.is_empty() {
        return Err(SecretError::EmptySecret);
    }
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0_u8; NONCE_LENGTH];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| SecretError::Random)?;

    let cipher = cipher(key);
    let mut encrypted = Zeroizing::new(plaintext.as_bytes().to_vec());
    let tag = cipher
        .seal_in_place_separate_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut encrypted,
        )
        .map_err(|_| SecretError::Random)?;

    let mut output = Vec::with_capacity(1 + NONCE_LENGTH + encrypted.len() + tag.as_ref().len());
    output.push(format_version);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&encrypted);
    output.extend_from_slice(tag.as_ref());
    let encoded = STANDARD.encode(&output);
    output.zeroize();
    Ok(encoded)
}

pub fn decrypt_for_this_machine(encoded: &str) -> Result<Zeroizing<String>, SecretError> {
    if let Some(encoded) = encoded.trim().strip_prefix("v3:") {
        let master = master_key()?;
        let key = derive_hkdf_context_key(&master, PASSWORD_KEY_CONTEXT)?;
        return decrypt_with_key(encoded, &key, MASTER_FORMAT_VERSION);
    }
    if let Some(encoded) = encoded.trim().strip_prefix("v2:") {
        let key = master_key()?;
        return decrypt_with_key(encoded, &key, LEGACY_MASTER_FORMAT_VERSION);
    }
    let key = machine_key()?;
    decrypt_with_key(encoded, &key, LEGACY_FORMAT_VERSION)
}

fn decrypt_with_key(
    encoded: &str,
    key: &[u8; 32],
    format_version: u8,
) -> Result<Zeroizing<String>, SecretError> {
    let mut decoded = Zeroizing::new(
        STANDARD
            .decode(encoded.trim())
            .map_err(|_| SecretError::InvalidBase64)?,
    );
    if decoded.len() < MIN_NON_EMPTY_CIPHERTEXT_BYTES || decoded[0] != format_version {
        return Err(SecretError::InvalidFormat);
    }

    let mut nonce_bytes = [0_u8; NONCE_LENGTH];
    nonce_bytes.copy_from_slice(&decoded[1..1 + NONCE_LENGTH]);
    let mut encrypted = Zeroizing::new(decoded.split_off(1 + NONCE_LENGTH));
    let cipher = cipher(key);
    let plaintext = cipher
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut encrypted,
        )
        .map_err(|_| SecretError::Decryption)?;
    let plaintext_len = plaintext.len();
    encrypted.truncate(plaintext_len);
    let bytes = std::mem::take(&mut *encrypted);
    String::from_utf8(bytes)
        .map(Zeroizing::new)
        .map_err(|error| {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            SecretError::InvalidText
        })
}

pub fn initialize_master_key(path: &Path) -> Result<(), SecretError> {
    let rng = SystemRandom::new();
    let mut key = Zeroizing::new([0_u8; 32]);
    rng.fill(&mut *key).map_err(|_| SecretError::Random)?;
    let encoded = format!("{}\n", STANDARD.encode(*key));
    write_new(path, encoded.as_bytes()).map_err(SecretError::MasterKeyIo)
}

pub fn random_nonce() -> Result<String, SecretError> {
    let rng = SystemRandom::new();
    let mut nonce = Zeroizing::new([0_u8; 32]);
    rng.fill(&mut *nonce).map_err(|_| SecretError::Random)?;
    Ok(STANDARD.encode(*nonce))
}

pub fn sign_authorization_payload(payload: &[u8]) -> Result<String, SecretError> {
    let key = authorization_key()?;
    Ok(sign_payload_with_key(&key, payload))
}

pub fn verify_authorization_payload(payload: &[u8], signature: &str) -> Result<bool, SecretError> {
    let signature = STANDARD
        .decode(signature)
        .map_err(|_| SecretError::InvalidBase64)?;
    let key = authorization_key()?;
    Ok(verify_payload_with_key(&key, payload, &signature))
}

pub fn verify_authorization_payload_from(
    master_key_path: &Path,
    payload: &[u8],
    signature: &str,
) -> Result<bool, SecretError> {
    let signature = STANDARD
        .decode(signature)
        .map_err(|_| SecretError::InvalidBase64)?;
    let key = derived_master_key_from(master_key_path, AUTH_KEY_CONTEXT)?;
    Ok(verify_payload_with_key(&key, payload, &signature))
}

pub fn sign_dataset_manifest_payload(payload: &[u8]) -> Result<String, SecretError> {
    let key = derived_master_key(DATASET_KEY_CONTEXT)?;
    Ok(sign_payload_with_key(&key, payload))
}

pub fn verify_dataset_manifest_payload(
    payload: &[u8],
    signature: &str,
) -> Result<bool, SecretError> {
    let signature = STANDARD
        .decode(signature)
        .map_err(|_| SecretError::InvalidBase64)?;
    let key = derived_master_key(DATASET_KEY_CONTEXT)?;
    Ok(verify_payload_with_key(&key, payload, &signature))
}

pub fn sign_release_readiness_payload(payload: &[u8]) -> Result<String, SecretError> {
    let key = derived_master_key(RELEASE_READINESS_KEY_CONTEXT)?;
    Ok(sign_payload_with_key(&key, payload))
}

pub fn sign_release_readiness_payload_from(
    master_key_path: &Path,
    payload: &[u8],
) -> Result<String, SecretError> {
    let key = derived_master_key_from(master_key_path, RELEASE_READINESS_KEY_CONTEXT)?;
    Ok(sign_payload_with_key(&key, payload))
}

pub fn verify_release_readiness_payload_from(
    master_key_path: &Path,
    payload: &[u8],
    signature: &str,
) -> Result<bool, SecretError> {
    let signature = STANDARD
        .decode(signature)
        .map_err(|_| SecretError::InvalidBase64)?;
    let key = derived_master_key_from(master_key_path, RELEASE_READINESS_KEY_CONTEXT)?;
    Ok(verify_payload_with_key(&key, payload, &signature))
}

pub fn journal_authentication_key() -> Result<Zeroizing<[u8; 32]>, SecretError> {
    derived_master_key(JOURNAL_KEY_CONTEXT)
}

pub fn journal_authentication_key_from(
    master_key_path: &Path,
) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let master = master_key_from(master_key_path)?;
    Ok(derive_context_key(&master, JOURNAL_KEY_CONTEXT))
}

fn sign_payload_with_key(key: &[u8; 32], payload: &[u8]) -> String {
    STANDARD.encode(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), payload).as_ref())
}

fn verify_payload_with_key(key: &[u8; 32], payload: &[u8], signature: &[u8]) -> bool {
    hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, key), payload, signature).is_ok()
}

pub fn uses_live_key_format(encoded: &str) -> bool {
    encoded.trim().starts_with("v3:")
}

fn authorization_key() -> Result<Zeroizing<[u8; 32]>, SecretError> {
    derived_master_key(AUTH_KEY_CONTEXT)
}

fn derived_master_key(context_label: &[u8]) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let master = master_key()?;
    Ok(derive_context_key(&master, context_label))
}

fn derived_master_key_from(
    master_key_path: &Path,
    context_label: &[u8],
) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let master = master_key_from(master_key_path)?;
    Ok(derive_context_key(&master, context_label))
}

fn derive_context_key(master: &[u8; 32], context_label: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(context_label);
    context.update(master);
    let hash = context.finish();
    let mut key = Zeroizing::new([0_u8; 32]);
    key.copy_from_slice(hash.as_ref());
    key
}

#[derive(Clone, Copy)]
struct HkdfKeyLength;

impl hkdf::KeyType for HkdfKeyLength {
    fn len(&self) -> usize {
        32
    }
}

fn derive_hkdf_context_key(
    master: &[u8; 32],
    context_label: &[u8],
) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT);
    let pseudo_random_key = salt.extract(master);
    let info = [context_label];
    let output = pseudo_random_key
        .expand(&info, HkdfKeyLength)
        .map_err(|_| SecretError::InvalidMasterKey)?;
    let mut key = Zeroizing::new([0_u8; 32]);
    output
        .fill(&mut *key)
        .map_err(|_| SecretError::InvalidMasterKey)?;
    Ok(key)
}

fn master_key() -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let path = env::var(MASTER_KEY_ENV).map_err(|_| SecretError::MasterKeyMissing)?;
    master_key_from(Path::new(&path))
}

fn master_key_from(path: &Path) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let encoded =
        crate::secure_fs::read_private_limited(path, MAX_KEY_FILE_BYTES).map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                SecretError::InsecureMasterKeyPermissions
            } else {
                SecretError::MasterKeyIo(error)
            }
        })?;
    let encoded = std::str::from_utf8(&encoded).map_err(|_| SecretError::InvalidMasterKey)?;
    let decoded = Zeroizing::new(
        STANDARD
            .decode(encoded.trim())
            .map_err(|_| SecretError::InvalidMasterKey)?,
    );
    if decoded.len() != 32 {
        return Err(SecretError::InvalidMasterKey);
    }
    let mut key = Zeroizing::new([0_u8; 32]);
    key.copy_from_slice(&decoded);
    Ok(key)
}

fn machine_key() -> Result<Zeroizing<[u8; 32]>, SecretError> {
    machine_key_from(Path::new(MACHINE_ID_PATH))
}

fn machine_key_from(path: &Path) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let mut machine_id = Zeroizing::new(
        crate::secure_fs::read_limited(path, MAX_MACHINE_ID_BYTES)
            .map_err(SecretError::MachineId)?,
    );
    while machine_id.last().is_some_and(u8::is_ascii_whitespace) {
        machine_id.pop();
    }
    if machine_id.is_empty() {
        return Err(SecretError::EmptyMachineId);
    }
    let mut material = digest::Context::new(&digest::SHA256);
    material.update(LEGACY_KEY_CONTEXT);
    material.update(&machine_id);
    let hash = material.finish();
    let mut key = Zeroizing::new([0_u8; 32]);
    key.copy_from_slice(hash.as_ref());
    Ok(key)
}

#[cfg(test)]
pub(crate) fn encrypt_legacy_for_test(plaintext: &str) -> String {
    encrypt_with_key(plaintext, &machine_key().unwrap(), LEGACY_FORMAT_VERSION).unwrap()
}

fn cipher(key: &[u8; 32]) -> LessSafeKey {
    let key = UnboundKey::new(&aead::AES_256_GCM, key)
        .expect("AES-256-GCM siempre acepta claves de 32 bytes");
    LessSafeKey::new(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_master_key() -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "options-secret-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("master.key");
        crate::secure_fs::write_new(
            &path,
            format!("{}\n", STANDARD.encode([17_u8; 32])).as_bytes(),
        )
        .unwrap();
        path
    }

    #[test]
    fn round_trip_uses_base64_and_random_nonce() {
        let key = [7_u8; 32];
        let first = encrypt_with_key("una contraseña", &key, MASTER_FORMAT_VERSION).unwrap();
        let second = encrypt_with_key("una contraseña", &key, MASTER_FORMAT_VERSION).unwrap();

        assert_ne!(first, second);
        assert_eq!(
            &*decrypt_with_key(&first, &key, MASTER_FORMAT_VERSION).unwrap(),
            "una contraseña"
        );
        assert_eq!(
            &*decrypt_with_key(&second, &key, MASTER_FORMAT_VERSION).unwrap(),
            "una contraseña"
        );
        assert!(matches!(
            encrypt_with_key("", &key, MASTER_FORMAT_VERSION),
            Err(SecretError::EmptySecret)
        ));

        let nonce = [0_u8; NONCE_LENGTH];
        let mut empty = Vec::new();
        cipher(&key)
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut empty,
            )
            .unwrap();
        let mut encoded_empty = vec![MASTER_FORMAT_VERSION];
        encoded_empty.extend_from_slice(&nonce);
        encoded_empty.extend_from_slice(&empty);
        assert_eq!(encoded_empty.len(), MIN_NON_EMPTY_CIPHERTEXT_BYTES - 1);
        assert!(matches!(
            decrypt_with_key(&STANDARD.encode(encoded_empty), &key, MASTER_FORMAT_VERSION),
            Err(SecretError::InvalidFormat)
        ));
    }

    #[test]
    fn environment_wrappers_use_the_external_master_key_and_v2_format() {
        let _guard = MASTER_KEY_ENVIRONMENT_LOCK.lock().unwrap();
        let previous = std::env::var_os(MASTER_KEY_ENV);
        let path = test_master_key();
        unsafe { std::env::set_var(MASTER_KEY_ENV, &path) };

        let encoded = encrypt_for_this_machine("contraseña operativa").unwrap();
        assert!(encoded.starts_with("v3:"));
        assert_eq!(
            &*decrypt_for_this_machine(&encoded).unwrap(),
            "contraseña operativa"
        );

        let master = master_key_from(&path).unwrap();
        let legacy_v2 =
            encrypt_with_key("migración v2", &master, LEGACY_MASTER_FORMAT_VERSION).unwrap();
        assert_eq!(
            &*decrypt_for_this_machine(&format!("v2:{legacy_v2}")).unwrap(),
            "migración v2"
        );
        assert!(!uses_live_key_format(&format!("v2:{legacy_v2}")));

        let payload = b"account=1;limit=100";
        let authorization = sign_authorization_payload(payload).unwrap();
        assert!(verify_authorization_payload(payload, &authorization).unwrap());
        assert!(!verify_authorization_payload(b"account=1;limit=101", &authorization).unwrap());
        assert!(matches!(
            verify_authorization_payload(payload, "%%%"),
            Err(SecretError::InvalidBase64)
        ));
        assert_eq!(
            STANDARD.encode(*authorization_key().unwrap()),
            "cIUJ8aXfeLjMvSrw1RY5i4Wf/JEj5UpcTHPEjdhid/I="
        );

        let dataset = sign_dataset_manifest_payload(payload).unwrap();
        assert!(verify_dataset_manifest_payload(payload, &dataset).unwrap());
        assert!(!verify_dataset_manifest_payload(b"changed", &dataset).unwrap());

        let readiness = sign_release_readiness_payload(payload).unwrap();
        assert_eq!(
            readiness,
            sign_release_readiness_payload_from(&path, payload).unwrap()
        );
        assert!(verify_release_readiness_payload_from(&path, payload, &readiness).unwrap());

        let journal = journal_authentication_key().unwrap();
        assert_eq!(&*journal, &*journal_authentication_key_from(&path).unwrap());
        assert_eq!(
            STANDARD.encode(*journal),
            "W4M/dZ0VMzjdUzCamehphznjk6psw/KjT0dyg8YYLUQ="
        );

        match previous {
            Some(value) => unsafe { std::env::set_var(MASTER_KEY_ENV, value) },
            None => unsafe { std::env::remove_var(MASTER_KEY_ENV) },
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn changed_ciphertext_is_rejected() {
        let key = [9_u8; 32];
        let encoded = encrypt_with_key("secreto", &key, MASTER_FORMAT_VERSION).unwrap();
        let mut bytes = STANDARD.decode(encoded).unwrap();
        let last = bytes.last_mut().unwrap();
        *last ^= 1;

        assert!(matches!(
            decrypt_with_key(&STANDARD.encode(bytes), &key, MASTER_FORMAT_VERSION),
            Err(SecretError::Decryption)
        ));
    }

    #[test]
    fn authorization_signature_rejects_any_payload_change() {
        let key = [11_u8; 32];
        let signature = sign_payload_with_key(&key, b"account=1;limit=100");
        let signature = STANDARD.decode(signature).unwrap();

        assert!(verify_payload_with_key(
            &key,
            b"account=1;limit=100",
            &signature
        ));
        assert!(!verify_payload_with_key(
            &key,
            b"account=1;limit=101",
            &signature
        ));
    }

    #[test]
    fn explicit_master_key_signatures_are_context_separated_and_fail_closed() {
        let path = test_master_key();
        let payload = b"build=abc;evidence=def";
        let readiness = sign_release_readiness_payload_from(&path, payload).unwrap();
        assert!(verify_release_readiness_payload_from(&path, payload, &readiness).unwrap());
        assert!(!verify_release_readiness_payload_from(&path, b"changed", &readiness).unwrap());
        assert!(matches!(
            verify_release_readiness_payload_from(&path, payload, "not-base64!"),
            Err(SecretError::InvalidBase64)
        ));

        let authorization_key = derived_master_key_from(&path, AUTH_KEY_CONTEXT).unwrap();
        let authorization = sign_payload_with_key(&authorization_key, payload);
        assert!(verify_authorization_payload_from(&path, payload, &authorization).unwrap());
        assert!(!verify_authorization_payload_from(&path, payload, &readiness).unwrap());

        let directory = path.parent().unwrap().to_path_buf();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn password_hkdf_has_an_independent_versioned_vector() {
        let master = [17_u8; 32];
        let password_key = derive_hkdf_context_key(&master, PASSWORD_KEY_CONTEXT).unwrap();
        assert_eq!(
            STANDARD.encode(*password_key),
            "cK9/L51AnfRQIP2EjEU6i1LFLQN36axBRihAyNaHZng="
        );
        assert_ne!(
            &*password_key,
            &*derive_hkdf_context_key(&master, AUTH_KEY_CONTEXT).unwrap()
        );
    }

    #[test]
    fn master_key_initialization_creates_exact_private_material_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generated-master.key");
        initialize_master_key(&path).unwrap();

        let encoded = crate::secure_fs::read_private_limited(&path, MAX_KEY_FILE_BYTES).unwrap();
        let decoded = STANDARD
            .decode(std::str::from_utf8(&encoded).unwrap().trim())
            .unwrap();
        assert_eq!(decoded.len(), 32);
        assert!(matches!(
            initialize_master_key(&path),
            Err(SecretError::MasterKeyIo(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
    }

    #[test]
    fn malformed_ciphertext_and_non_utf8_plaintext_fail_with_precise_errors() {
        let key = [23_u8; 32];
        assert!(matches!(
            decrypt_with_key("%%%", &key, MASTER_FORMAT_VERSION),
            Err(SecretError::InvalidBase64)
        ));
        assert!(matches!(
            decrypt_with_key(
                &STANDARD.encode([MASTER_FORMAT_VERSION]),
                &key,
                MASTER_FORMAT_VERSION
            ),
            Err(SecretError::InvalidFormat)
        ));
        let wrong_version = encrypt_with_key("secret", &key, LEGACY_FORMAT_VERSION).unwrap();
        assert!(matches!(
            decrypt_with_key(&wrong_version, &key, MASTER_FORMAT_VERSION),
            Err(SecretError::InvalidFormat)
        ));

        let nonce = [5_u8; NONCE_LENGTH];
        let mut encrypted = vec![0xff];
        cipher(&key)
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut encrypted,
            )
            .unwrap();
        let mut encoded = vec![MASTER_FORMAT_VERSION];
        encoded.extend_from_slice(&nonce);
        encoded.extend_from_slice(&encrypted);
        assert!(matches!(
            decrypt_with_key(&STANDARD.encode(encoded), &key, MASTER_FORMAT_VERSION),
            Err(SecretError::InvalidText)
        ));
    }

    #[test]
    fn master_and_machine_key_files_reject_every_malformed_boundary() {
        assert_eq!(MAX_KEY_FILE_BYTES, 4_096);
        assert_eq!(MAX_MACHINE_ID_BYTES, 4_096);
        let valid = test_master_key();
        let directory = valid.parent().unwrap();
        assert_eq!(&*master_key_from(&valid).unwrap(), &[17_u8; 32]);

        let invalid_base64 = directory.join("invalid-base64.key");
        crate::secure_fs::write_new(&invalid_base64, b"%%%\n").unwrap();
        assert!(matches!(
            master_key_from(&invalid_base64),
            Err(SecretError::InvalidMasterKey)
        ));
        let wrong_length = directory.join("wrong-length.key");
        crate::secure_fs::write_new(&wrong_length, STANDARD.encode([1_u8; 31]).as_bytes()).unwrap();
        assert!(matches!(
            master_key_from(&wrong_length),
            Err(SecretError::InvalidMasterKey)
        ));
        let invalid_text = directory.join("invalid-text.key");
        crate::secure_fs::write_new(&invalid_text, &[0xff]).unwrap();
        assert!(matches!(
            master_key_from(&invalid_text),
            Err(SecretError::InvalidMasterKey)
        ));
        assert!(matches!(
            master_key_from(&directory.join("missing.key")),
            Err(SecretError::MasterKeyIo(_))
        ));

        let empty_machine = directory.join("empty-machine-id");
        std::fs::write(&empty_machine, b" \n\t").unwrap();
        assert!(matches!(
            machine_key_from(&empty_machine),
            Err(SecretError::EmptyMachineId)
        ));
        assert!(matches!(
            machine_key_from(&directory.join("missing-machine-id")),
            Err(SecretError::MachineId(_))
        ));
        assert_eq!(
            &*machine_key().unwrap(),
            &*machine_key_from(Path::new(MACHINE_ID_PATH)).unwrap()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn broad_master_key_permissions_are_reported_explicitly() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_master_key();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            master_key_from(&path),
            Err(SecretError::InsecureMasterKeyPermissions)
        ));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn nonce_and_format_detection_have_closed_shapes() {
        let nonce = random_nonce().unwrap();
        assert_eq!(STANDARD.decode(nonce).unwrap().len(), 32);
        assert!(uses_live_key_format("  v3:payload"));
        assert!(!uses_live_key_format("v2:payload"));
        assert!(!uses_live_key_format("v1:payload"));
    }
}
