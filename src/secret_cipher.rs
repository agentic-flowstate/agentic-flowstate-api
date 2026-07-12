use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{bail, Context, Result};
use once_cell::sync::OnceCell;
use rand::RngCore;
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

static MASTER_KEY: OnceCell<[u8; 32]> = OnceCell::new();

pub fn encrypt(plaintext: &str, aad: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    if plaintext.is_empty() || plaintext.len() > 32_768 {
        bail!("secret value must contain 1 to 32768 bytes");
    }
    let cipher = Aes256Gcm::new_from_slice(master_key()?).context("Invalid secret-store key")?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            aes_gcm::aead::Payload {
                msg: plaintext.as_bytes(),
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("Failed to encrypt secret value"))?;
    Ok((ciphertext, nonce.to_vec()))
}

#[allow(dead_code)]
pub fn decrypt(ciphertext: &[u8], nonce: &[u8], aad: &[u8]) -> Result<String> {
    if nonce.len() != 12 {
        bail!("Invalid secret nonce");
    }
    let cipher = Aes256Gcm::new_from_slice(master_key()?).context("Invalid secret-store key")?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            aes_gcm::aead::Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("Failed to decrypt secret value"))?;
    String::from_utf8(plaintext).context("Secret value is not valid UTF-8")
}

pub fn aad(user_id: &str, organization: &str, key: &str) -> Vec<u8> {
    format!("agentic-secret-v1\0{user_id}\0{organization}\0{key}").into_bytes()
}

fn master_key() -> Result<&'static [u8; 32]> {
    MASTER_KEY.get_or_try_init(load_or_create_master_key)
}

fn load_or_create_master_key() -> Result<[u8; 32]> {
    let path = key_path()?;
    if path.exists() {
        let metadata = fs::metadata(&path).context("Failed to inspect secret-store key")?;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("Secret-store key permissions must be 0600");
        }
        let bytes = fs::read(&path).context("Failed to read secret-store key")?;
        return bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Secret-store key must be exactly 32 bytes"));
    }
    let parent = path
        .parent()
        .context("Secret-store key path has no parent")?;
    fs::create_dir_all(parent).context("Failed to create secret-store directory")?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .context("Failed to secure secret-store directory")?;
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .context("Failed to create secret-store key")?;
    file.write_all(&key)
        .context("Failed to write secret-store key")?;
    file.sync_all().context("Failed to sync secret-store key")?;
    Ok(key)
}

fn key_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Could not determine home directory")?
        .join(".agentic-flowstate/secrets/master.key"))
}
