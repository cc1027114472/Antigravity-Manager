use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Deserializer, Serializer};
use sha2::Digest;

const LEGACY_FIXED_NONCE: &[u8; 12] = b"antigravsalt";
const ENCRYPTED_PREFIX: &str = "ag_enc_";
const ENCRYPTED_V2_PREFIX: &str = "ag_enc_v2_";

/// 获取用于加密的主密钥（优先从持久化数据目录读取，不存在则固化当前机器码）
fn get_encryption_key() -> [u8; 32] {
    if let Ok(data_dir) = crate::modules::account::get_data_dir() {
        let key_file = data_dir.join("device_id.key");
        if let Ok(content) = std::fs::read_to_string(&key_file) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                let mut key = [0u8; 32];
                let hash = sha2::Sha256::digest(trimmed.as_bytes());
                key.copy_from_slice(&hash);
                return key;
            }
        }
        // 如果文件不存在，则将当前机器码固化到持久化目录，避免后续容器重建导致密钥丢失
        let current_uid = machine_uid::get().unwrap_or_else(|_| "default".to_string());
        let _ = std::fs::write(&key_file, current_uid.trim());
        let mut key = [0u8; 32];
        let hash = sha2::Sha256::digest(current_uid.as_bytes());
        key.copy_from_slice(&hash);
        return key;
    }

    let device_id = machine_uid::get().unwrap_or_else(|_| "default".to_string());
    let mut key = [0u8; 32];
    let hash = sha2::Sha256::digest(device_id.as_bytes());
    key.copy_from_slice(&hash);
    key
}

/// 获取所有候选解密密钥（持久化密钥 -> 当前机器码 -> 默认机器码），确保跨环境/重建也能平滑解密
fn get_candidate_keys() -> Vec<[u8; 32]> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. 持久化密钥
    let primary = get_encryption_key();
    if seen.insert(primary) {
        candidates.push(primary);
    }

    // 2. 当前运行时机器码
    let current_uid = machine_uid::get().unwrap_or_else(|_| "default".to_string());
    let mut key = [0u8; 32];
    let hash = sha2::Sha256::digest(current_uid.as_bytes());
    key.copy_from_slice(&hash);
    if seen.insert(key) {
        candidates.push(key);
    }

    // 3. 兜底 "default"
    let mut def_key = [0u8; 32];
    let hash = sha2::Sha256::digest(b"default");
    def_key.copy_from_slice(&hash);
    if seen.insert(def_key) {
        candidates.push(def_key);
    }

    candidates
}

pub fn serialize_password<S>(password: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if password.starts_with(ENCRYPTED_PREFIX) || password.starts_with(ENCRYPTED_V2_PREFIX) {
        return serializer.serialize_str(password);
    }

    let encrypted = encrypt_string(password).map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(&encrypted)
}

pub fn deserialize_password<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    if raw.is_empty() {
        return Ok(raw);
    }

    if raw.starts_with(ENCRYPTED_V2_PREFIX) {
        match decrypt_string_v2(&raw[ENCRYPTED_V2_PREFIX.len()..]) {
            Ok(plaintext) => Ok(plaintext),
            Err(_) => Ok(raw),
        }
    } else if raw.starts_with(ENCRYPTED_PREFIX) {
        match decrypt_legacy(&raw[ENCRYPTED_PREFIX.len()..]) {
            Ok(plaintext) => Ok(plaintext),
            Err(_) => Ok(raw),
        }
    } else {
        match decrypt_legacy(&raw) {
            Ok(plaintext) => Ok(plaintext),
            Err(_) => Ok(raw),
        }
    }
}

pub fn encrypt_string(password: &str) -> Result<String, String> {
    let key = get_encryption_key();
    let cipher = Aes256Gcm::new(&key.into());

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, password.as_bytes())
        .map_err(|e| format!("Encryption failed: {}", e))?;

    let encoded_nonce = general_purpose::STANDARD_NO_PAD.encode(nonce_bytes);
    let encoded_ciphertext = general_purpose::STANDARD_NO_PAD.encode(ciphertext);
    Ok(format!(
        "{}{}.{}",
        ENCRYPTED_V2_PREFIX, encoded_nonce, encoded_ciphertext
    ))
}

fn decrypt_legacy(encrypted_base64: &str) -> Result<String, String> {
    let ciphertext = general_purpose::STANDARD
        .decode(encrypted_base64)
        .map_err(|e| format!("Base64 decode failed: {}", e))?;
    let nonce = Nonce::from_slice(LEGACY_FIXED_NONCE);

    let mut last_err = "Decryption failed".to_string();
    for key in get_candidate_keys() {
        let cipher = Aes256Gcm::new(&key.into());
        match cipher.decrypt(nonce, ciphertext.as_ref()) {
            Ok(plaintext) => {
                return String::from_utf8(plaintext)
                    .map_err(|e| format!("UTF-8 conversion failed: {}", e));
            }
            Err(e) => {
                last_err = format!("Decryption failed: {}", e);
            }
        }
    }

    Err(last_err)
}

fn decrypt_string_v2(encrypted: &str) -> Result<String, String> {
    let (nonce_base64, ciphertext_base64) = encrypted
        .split_once('.')
        .ok_or_else(|| "Invalid encrypted payload".to_string())?;

    let nonce_bytes = general_purpose::STANDARD_NO_PAD
        .decode(nonce_base64)
        .map_err(|e| format!("Nonce decode failed: {}", e))?;
    if nonce_bytes.len() != 12 {
        return Err("Invalid nonce length".to_string());
    }

    let ciphertext = general_purpose::STANDARD_NO_PAD
        .decode(ciphertext_base64)
        .map_err(|e| format!("Ciphertext decode failed: {}", e))?;

    let nonce = Nonce::from_slice(&nonce_bytes);
    let mut last_err = "Decryption failed".to_string();
    for key in get_candidate_keys() {
        let cipher = Aes256Gcm::new(&key.into());
        match cipher.decrypt(nonce, ciphertext.as_ref()) {
            Ok(plaintext) => {
                return String::from_utf8(plaintext)
                    .map_err(|e| format!("UTF-8 conversion failed: {}", e));
            }
            Err(e) => {
                last_err = format!("Decryption failed: {}", e);
            }
        }
    }

    Err(last_err)
}

pub fn decrypt_string(encrypted: &str) -> Result<String, String> {
    if encrypted.starts_with(ENCRYPTED_V2_PREFIX) {
        decrypt_string_v2(&encrypted[ENCRYPTED_V2_PREFIX.len()..])
    } else if encrypted.starts_with(ENCRYPTED_PREFIX) {
        decrypt_legacy(&encrypted[ENCRYPTED_PREFIX.len()..])
    } else {
        decrypt_legacy(encrypted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_cycle() {
        let password = "my_secret_password";
        let encrypted = encrypt_string(password).unwrap();

        assert!(encrypted.starts_with(ENCRYPTED_V2_PREFIX));
        assert_ne!(password, encrypted);

        let decrypted = decrypt_string(&encrypted).unwrap();
        assert_eq!(password, decrypted);
    }

    #[test]
    fn test_encrypt_uses_unique_nonce() {
        let password = "my_secret_password";
        let encrypted_a = encrypt_string(password).unwrap();
        let encrypted_b = encrypt_string(password).unwrap();

        assert_ne!(encrypted_a, encrypted_b);
        assert_eq!(decrypt_string(&encrypted_a).unwrap(), password);
        assert_eq!(decrypt_string(&encrypted_b).unwrap(), password);
    }

    #[test]
    fn test_legacy_compatibility() {
        let password = "legacy_password";
        let key = get_encryption_key();
        let cipher = Aes256Gcm::new(&key.into());
        let nonce = Nonce::from_slice(LEGACY_FIXED_NONCE);
        let ciphertext = cipher.encrypt(nonce, password.as_bytes()).unwrap();
        let legacy_encrypted = general_purpose::STANDARD.encode(ciphertext);

        assert!(!legacy_encrypted.starts_with(ENCRYPTED_PREFIX));

        let decrypted = decrypt_string(&legacy_encrypted).unwrap();
        assert_eq!(password, decrypted);
    }
}
