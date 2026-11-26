//! BLS12-381 "min-sig" (signatures in G1, 48 bytes compressed; public keys in G2, 96 bytes
//! compressed), as @noble/curves' short signatures and blst's min_sig, plus the EIP-2537
//! encodings the contracts take.

use blst::min_sig::{AggregatePublicKey, AggregateSignature, PublicKey, SecretKey, Signature};
use blst::BLST_ERROR;

use crate::error::{Error, Result};

fn unhex(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).map_err(|_| Error::invalid("not hex"))
}

fn secret(secret_hex: &str) -> Result<SecretKey> {
    SecretKey::from_bytes(&unhex(secret_hex)?).map_err(|e| Error::internal(format!("not a BLS12-381 secret key: {e:?}")))
}

/// A fresh key pair: (secret hex, compressed public key hex).
pub fn new_key() -> (String, String) {
    let mut ikm = [0u8; 32];
    rand::fill(&mut ikm);
    let sk = SecretKey::key_gen(&ikm, &[]).expect("32 bytes of key material");
    (hex::encode(sk.to_bytes()), hex::encode(sk.sk_to_pk().compress()))
}

pub fn public_key_of(secret_hex: &str) -> Result<String> {
    Ok(hex::encode(secret(secret_hex)?.sk_to_pk().compress()))
}

/// Sign `msg` (hashed to G1 under `dst`); the compressed signature as hex.
pub fn sign(secret_hex: &str, msg: &[u8], dst: &[u8]) -> Result<String> {
    Ok(hex::encode(secret(secret_hex)?.sign(msg, dst, &[]).compress()))
}

fn public_key(pk_hex: &str) -> Option<PublicKey> {
    let pk = PublicKey::from_bytes(&unhex(pk_hex).ok()?).ok()?;
    pk.validate().ok()?;
    Some(pk)
}

pub fn valid_public_key(pk_hex: &str) -> bool {
    pk_hex.len() == 192 && pk_hex.chars().all(|c| c.is_ascii_hexdigit()) && public_key(pk_hex).is_some()
}

fn verify_with(sig_hex: &str, msg: &[u8], dst: &[u8], pk: &PublicKey) -> bool {
    let Ok(bytes) = unhex(sig_hex) else { return false };
    let Ok(sig) = Signature::from_bytes(&bytes) else { return false };
    sig.verify(true, msg, dst, &[], pk, false) == BLST_ERROR::BLST_SUCCESS
}

pub fn verify(sig_hex: &str, msg: &[u8], dst: &[u8], pk_hex: &str) -> bool {
    public_key(pk_hex).is_some_and(|pk| verify_with(sig_hex, msg, dst, &pk))
}

/// Verify an aggregate signature against the sum of `pks`.
pub fn verify_aggregate(sig_hex: &str, msg: &[u8], dst: &[u8], pks: &[String]) -> bool {
    let keys: Option<Vec<PublicKey>> = pks.iter().map(|p| public_key(p)).collect();
    let Some(keys) = keys.filter(|k| !k.is_empty()) else { return false };
    let refs: Vec<&PublicKey> = keys.iter().collect();
    let Ok(agg) = AggregatePublicKey::aggregate(&refs, false) else { return false };
    verify_with(sig_hex, msg, dst, &agg.to_public_key())
}

/// The sum of compressed signatures, compressed.
pub fn aggregate_signatures(sigs: &[String]) -> Result<String> {
    let parsed: Vec<Signature> = sigs.iter().map(|s| Signature::from_bytes(&unhex(s)?).map_err(|_| Error::invalid("bad signature"))).collect::<Result<_>>()?;
    let refs: Vec<&Signature> = parsed.iter().collect();
    let agg = AggregateSignature::aggregate(&refs, true).map_err(|e| Error::invalid(format!("can't aggregate: {e:?}")))?;
    Ok(hex::encode(agg.to_signature().compress()))
}

fn pad(fp: &[u8]) -> String {
    format!("{}{}", "00".repeat(16), hex::encode(fp))
}

/// A compressed G1 point (signature) in EIP-2537 encoding: x ‖ y, 64 bytes each (hex).
pub fn g1_eip(compressed_hex: &str) -> Result<String> {
    let sig = Signature::from_bytes(&unhex(compressed_hex)?).map_err(|_| Error::invalid("not a G1 point"))?;
    let raw = sig.serialize();
    Ok(format!("{}{}", pad(&raw[..48]), pad(&raw[48..])))
}

/// A compressed G2 point (public key) in EIP-2537 encoding: x.c0 ‖ x.c1 ‖ y.c0 ‖ y.c1 (hex).
pub fn g2_eip(compressed_hex: &str) -> Result<String> {
    let pk = public_key(compressed_hex).ok_or_else(|| Error::invalid("not a G2 public key"))?;
    // Uncompressed (ZCash order): x.c1 ‖ x.c0 ‖ y.c1 ‖ y.c0, 48 bytes each.
    let raw = pk.serialize();
    let fp = |i: usize| pad(&raw[i * 48..(i + 1) * 48]);
    Ok(format!("{}{}{}{}", fp(1), fp(0), fp(3), fp(2)))
}
