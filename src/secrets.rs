use std::{fs, path::Path};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    digest,
    rand::{SecureRandom, SystemRandom},
};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

const MACHINE_ID_PATH: &str = "/etc/machine-id";
const FORMAT_VERSION: u8 = 1;
const NONCE_LENGTH: usize = 12;
const KEY_CONTEXT: &[u8] = b"options-trading:iol-password:v1\0";

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("no se pudo leer {MACHINE_ID_PATH}: {0}")]
    MachineId(std::io::Error),
    #[error("{MACHINE_ID_PATH} está vacío")]
    EmptyMachineId,
    #[error("no se pudo generar un valor aleatorio seguro")]
    Random,
    #[error("el texto cifrado no es un Base64 válido")]
    InvalidBase64,
    #[error("el texto cifrado está incompleto o pertenece a otra versión")]
    InvalidFormat,
    #[error("no se pudo descifrar; comprobar que el valor fue creado en esta máquina")]
    Decryption,
    #[error("la contraseña descifrada no es texto válido")]
    InvalidText,
}

pub fn encrypt_for_this_machine(plaintext: &str) -> Result<String, SecretError> {
    let key = machine_key()?;
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0_u8; NONCE_LENGTH];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| SecretError::Random)?;

    let cipher = cipher(&key);
    let mut encrypted = Zeroizing::new(plaintext.as_bytes().to_vec());
    let tag = cipher
        .seal_in_place_separate_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut encrypted,
        )
        .map_err(|_| SecretError::Random)?;

    let mut output = Vec::with_capacity(1 + NONCE_LENGTH + encrypted.len() + tag.as_ref().len());
    output.push(FORMAT_VERSION);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&encrypted);
    output.extend_from_slice(tag.as_ref());
    let encoded = STANDARD.encode(&output);
    output.zeroize();
    Ok(encoded)
}

pub fn decrypt_for_this_machine(encoded: &str) -> Result<Zeroizing<String>, SecretError> {
    let mut decoded = Zeroizing::new(
        STANDARD
            .decode(encoded.trim())
            .map_err(|_| SecretError::InvalidBase64)?,
    );
    if decoded.len() <= 1 + NONCE_LENGTH + aead::AES_256_GCM.tag_len()
        || decoded[0] != FORMAT_VERSION
    {
        return Err(SecretError::InvalidFormat);
    }

    let mut nonce_bytes = [0_u8; NONCE_LENGTH];
    nonce_bytes.copy_from_slice(&decoded[1..1 + NONCE_LENGTH]);
    let mut encrypted = Zeroizing::new(decoded.split_off(1 + NONCE_LENGTH));
    let key = machine_key()?;
    let cipher = cipher(&key);
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

fn machine_key() -> Result<Zeroizing<[u8; 32]>, SecretError> {
    machine_key_from(Path::new(MACHINE_ID_PATH))
}

fn machine_key_from(path: &Path) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let mut machine_id = Zeroizing::new(fs::read(path).map_err(SecretError::MachineId)?);
    while machine_id.last().is_some_and(u8::is_ascii_whitespace) {
        machine_id.pop();
    }
    if machine_id.is_empty() {
        return Err(SecretError::EmptyMachineId);
    }
    let mut material = digest::Context::new(&digest::SHA256);
    material.update(KEY_CONTEXT);
    material.update(&machine_id);
    let hash = material.finish();
    let mut key = Zeroizing::new([0_u8; 32]);
    key.copy_from_slice(hash.as_ref());
    Ok(key)
}

fn cipher(key: &[u8; 32]) -> LessSafeKey {
    let key = UnboundKey::new(&aead::AES_256_GCM, key)
        .expect("AES-256-GCM siempre acepta claves de 32 bytes");
    LessSafeKey::new(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_uses_base64_and_random_nonce() {
        let first = encrypt_for_this_machine("una contraseña").unwrap();
        let second = encrypt_for_this_machine("una contraseña").unwrap();

        assert_ne!(first, second);
        assert_eq!(
            &*decrypt_for_this_machine(&first).unwrap(),
            "una contraseña"
        );
        assert_eq!(
            &*decrypt_for_this_machine(&second).unwrap(),
            "una contraseña"
        );
    }

    #[test]
    fn changed_ciphertext_is_rejected() {
        let encoded = encrypt_for_this_machine("secreto").unwrap();
        let mut bytes = STANDARD.decode(encoded).unwrap();
        let last = bytes.last_mut().unwrap();
        *last ^= 1;

        assert!(matches!(
            decrypt_for_this_machine(&STANDARD.encode(bytes)),
            Err(SecretError::Decryption)
        ));
    }
}
