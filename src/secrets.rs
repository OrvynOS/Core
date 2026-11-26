//! AES-256-GCM encryption at rest (keys derived from AI_KEY_ENCRYPTION_SECRET): creators' AI
//! provider API keys, and agents' OrvynPay shielded keys (a separate derived key per kind).
//! The format is the TypeScript core's: `v1:<iv b64>:<tag b64>:<ciphertext b64>`.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};

use crate::env::env;
use crate::error::{Error, Result};

const VERSION: &str = "v1";
const TAG_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    AiKey,
    ShieldedKey,
}

impl SecretKind {
    fn name(self) -> &'static str {
        match self {
            SecretKind::AiKey => "ai-key",
            SecretKind::ShieldedKey => "shielded-key",
        }
    }
}

fn key_from(secret: &str, kind: SecretKind) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    // AI keys keep the original derivation; every other kind gets its own key.
    if kind != SecretKind::AiKey {
        h.update(format!(":{}", kind.name()).as_bytes());
    }
    h.finalize().into()
}

/// `aad` binds the ciphertext to its owner (the agent id), so it can't be moved to another row.
pub fn encrypt_secret(plaintext: &str, aad: &str, kind: SecretKind) -> Result<String> {
    encrypt_with(&env().ai_key_encryption_secret, plaintext, aad, kind)
}

pub fn decrypt_secret(payload: &str, aad: &str, kind: SecretKind) -> Result<String> {
    decrypt_with(&env().ai_key_encryption_secret, payload, aad, kind)
}

pub fn encrypt_with(secret: &str, plaintext: &str, aad: &str, kind: SecretKind) -> Result<String> {
    let cipher = Aes256Gcm::new(&key_from(secret, kind).into());
    let iv: [u8; 12] = rand::random();
    let sealed = cipher
        .encrypt(Nonce::from_slice(&iv), Payload { msg: plaintext.as_bytes(), aad: aad.as_bytes() })
        .map_err(|_| Error::internal("encryption failed"))?;
    // aes-gcm appends the tag; the stored format keeps it separate.
    let (ciphertext, tag) = sealed.split_at(sealed.len() - TAG_LEN);
    Ok([VERSION.to_string(), B64.encode(iv), B64.encode(tag), B64.encode(ciphertext)].join(":"))
}

pub fn decrypt_with(secret: &str, payload: &str, aad: &str, kind: SecretKind) -> Result<String> {
    let parts: Vec<&str> = payload.split(':').collect();
    let bad = || Error::internal("unsupported secret format");
    if parts.len() < 4 || parts[0] != VERSION || parts[1..4].iter().any(|p| p.is_empty()) {
        return Err(bad());
    }
    let iv = B64.decode(parts[1]).map_err(|_| bad())?;
    let tag = B64.decode(parts[2]).map_err(|_| bad())?;
    let mut sealed = B64.decode(parts[3]).map_err(|_| bad())?;
    if iv.len() != 12 || tag.len() != TAG_LEN {
        return Err(bad());
    }
    sealed.extend_from_slice(&tag);
    let cipher = Aes256Gcm::new(&key_from(secret, kind).into());
    let plain = cipher
        .decrypt(Nonce::from_slice(&iv), Payload { msg: &sealed, aad: aad.as_bytes() })
        .map_err(|_| Error::internal("couldn't decrypt the secret"))?;
    String::from_utf8(plain).map_err(|_| bad())
}

/// A short, non-secret reminder of which key is stored, e.g. "sk-…a1b2".
pub fn secret_hint(secret: &str) -> String {
    let chars: Vec<char> = secret.trim().chars().collect();
    let head: String = chars.iter().take(3.min(chars.len())).collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    format!("{head}…{tail}")
}
