//! OrvynPay private mode, off chain: the same hashing, tree, keys and notes as the circuit
//! (contracts/circuits/transaction.circom) and the pool (OrvynShieldedPool.sol).
//!
//!   publicKey  = Poseidon(privateKey)
//!   commitment = Poseidon(amount, publicKey, blinding, asset)
//!   signature  = Poseidon(privateKey, commitment, leafIndex)
//!   nullifier  = Poseidon(commitment, leafIndex, signature)
//!   tree node  = Poseidon(left, right), empty leaf 0
//!
//! Notes are sent encrypted to their owner's encryption key (X25519, from the key registry):
//! ephemeral public key (32) | IV (12) | tag (16) | AES-256-GCM(amount | blinding | asset, 96 bytes).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use alloy::primitives::U256;
use ark_bn254::Fr;
use ark_ff::{BigInteger, PrimeField};
use light_poseidon::{Poseidon, PoseidonHasher};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::{Error, Result};

/// The BN254 scalar field.
pub const FIELD: U256 = U256::from_limbs([0x43e1f593f0000001, 0x2833e84879b97091, 0xb85045b68181585d, 0x30644e72e131a029]);

pub fn random_field() -> U256 {
    let mut b = [0u8; 32];
    rand::fill(&mut b[1..]); // 31 random bytes, as the TypeScript core
    U256::from_be_bytes(b)
}

pub fn field_of(x: U256) -> U256 {
    x % FIELD
}

fn fr(x: U256) -> Fr {
    Fr::from_be_bytes_mod_order(&x.to_be_bytes::<32>())
}

fn u256(f: Fr) -> U256 {
    U256::from_be_slice(&f.into_bigint().to_bytes_be())
}

/// Poseidon over BN254 with circom's parameters (poseidon-lite / circomlib).
pub fn poseidon(inputs: &[U256]) -> U256 {
    let mut h = Poseidon::<Fr>::new_circom(inputs.len()).expect("1 to 12 inputs");
    let xs: Vec<Fr> = inputs.iter().map(|x| fr(*x)).collect();
    u256(h.hash(&xs).expect("inputs in the field"))
}

pub fn hash2(a: U256, b: U256) -> U256 {
    poseidon(&[a, b])
}

// ---- Merkle tree (PoseidonTree.sol) ----

#[derive(Debug, Clone)]
pub struct MerkleTree {
    pub levels: usize,
    pub zeros: Vec<U256>,
    layers: Vec<Vec<U256>>,
}

impl MerkleTree {
    pub fn new(levels: usize, leaves: Vec<U256>) -> MerkleTree {
        let mut zeros = vec![];
        let mut z = U256::ZERO;
        for _ in 0..=levels {
            zeros.push(z);
            z = hash2(z, z);
        }
        let mut t = MerkleTree { levels, zeros, layers: vec![leaves] };
        t.rebuild();
        t
    }

    /// Recompute every layer above the leaves (as the TypeScript tree does).
    fn rebuild(&mut self) {
        self.layers.truncate(1);
        self.layers.resize(self.levels + 1, vec![]);
        for level in 1..=self.levels {
            let below = &self.layers[level - 1];
            let layer: Vec<U256> = (0..below.len().div_ceil(2)).map(|i| hash2(below[2 * i], if 2 * i + 1 < below.len() { below[2 * i + 1] } else { self.zeros[level - 1] })).collect();
            self.layers[level] = layer;
        }
    }

    pub fn leaves(&self) -> &[U256] {
        &self.layers[0]
    }

    pub fn insert(&mut self, leaves: &[U256]) {
        self.layers[0].extend_from_slice(leaves);
        self.rebuild();
    }

    pub fn root(&self) -> U256 {
        self.layers[self.levels].first().copied().unwrap_or(self.zeros[self.levels])
    }

    pub fn index_of(&self, leaf: U256) -> Option<usize> {
        self.layers[0].iter().position(|l| *l == leaf)
    }

    /// Sibling path for the leaf at `index` (bottom up), as the circuit's pathElements.
    pub fn path(&self, index: usize) -> Result<Vec<U256>> {
        if index >= self.layers[0].len() {
            return Err(Error::invalid("leaf not in tree"));
        }
        let mut i = index;
        let mut out = vec![];
        for level in 0..self.levels {
            let sibling = i ^ 1;
            out.push(self.layers[level].get(sibling).copied().unwrap_or(self.zeros[level]));
            i >>= 1;
        }
        Ok(out)
    }
}

// ---- Keys ----

#[derive(Debug, Clone)]
pub struct ShieldedKeys {
    /// Spending key (field element).
    pub private_key: U256,
    /// Poseidon(privateKey): registered in the pool, owner of notes.
    pub public_key: U256,
    /// X25519 key notes are encrypted to (raw 32 bytes, hex).
    pub encryption_private: String,
    /// 0x-prefixed hex.
    pub encryption_public: String,
}

pub fn public_key_of(private_key: U256) -> U256 {
    poseidon(&[private_key])
}

pub fn new_shielded_keys() -> ShieldedKeys {
    let private_key = random_field();
    let mut raw = [0u8; 32];
    rand::fill(&mut raw);
    let secret = StaticSecret::from(raw);
    let public = PublicKey::from(&secret);
    ShieldedKeys { private_key, public_key: public_key_of(private_key), encryption_private: hex::encode(secret.to_bytes()), encryption_public: format!("0x{}", hex::encode(public.as_bytes())) }
}

fn x25519_secret(hex_str: &str) -> Result<StaticSecret> {
    let b: [u8; 32] = hex::decode(hex_str.trim_start_matches("0x")).ok().and_then(|v| v.try_into().ok()).ok_or_else(|| Error::invalid("bad X25519 key"))?;
    Ok(StaticSecret::from(b))
}

fn x25519_public(bytes: &[u8]) -> Result<PublicKey> {
    let b: [u8; 32] = bytes.try_into().map_err(|_| Error::invalid("bad X25519 key"))?;
    Ok(PublicKey::from(b))
}

fn note_key(shared: &[u8], ephemeral: &[u8]) -> [u8; 32] {
    let hk = hkdf::Hkdf::<Sha256>::new(Some(ephemeral), shared);
    let mut out = [0u8; 32];
    hk.expand(b"orvyn-note-v1", &mut out).expect("32 bytes");
    out
}

// ---- Notes ----

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub amount: U256,
    /// Owner's shielded public key.
    pub public_key: U256,
    pub blinding: U256,
    /// uint160(token), 0 for BNB.
    pub asset: U256,
    /// Leaf index once in the tree.
    pub index: Option<usize>,
}

pub fn commitment_of(n: &Note) -> U256 {
    poseidon(&[n.amount, n.public_key, n.blinding, n.asset])
}

pub fn nullifier_of(n: &Note, private_key: U256) -> Result<U256> {
    let index = U256::from(n.index.ok_or_else(|| Error::invalid("note has no leaf index"))?);
    let commitment = commitment_of(n);
    let signature = poseidon(&[private_key, commitment, index]);
    Ok(poseidon(&[commitment, index, signature]))
}

/// Encrypt a note's secrets to its owner's encryption key (hex, as the pool event carries it).
pub fn encrypt_note(n: &Note, recipient_encryption_key: &str) -> Result<String> {
    encrypt_note_with(n, recipient_encryption_key, rand::random(), rand::random())
}

pub fn encrypt_note_with(n: &Note, recipient_encryption_key: &str, ephemeral_secret: [u8; 32], iv: [u8; 12]) -> Result<String> {
    let secret = StaticSecret::from(ephemeral_secret);
    let ephemeral = PublicKey::from(&secret);
    let recipient = x25519_public(&hex::decode(recipient_encryption_key.trim_start_matches("0x")).map_err(|_| Error::invalid("bad encryption key"))?)?;
    let shared = secret.diffie_hellman(&recipient);
    let key = note_key(shared.as_bytes(), ephemeral.as_bytes());
    let mut pt = Vec::with_capacity(96);
    for x in [n.amount, n.blinding, n.asset] {
        pt.extend(x.to_be_bytes::<32>());
    }
    let ct = Aes256Gcm::new_from_slice(&key).unwrap().encrypt(Nonce::from_slice(&iv), Payload { msg: &pt, aad: &[] }).map_err(|_| Error::internal("encryption failed"))?;
    // aes-gcm appends the tag; the wire format puts it before the ciphertext.
    let (body, tag) = ct.split_at(ct.len() - 16);
    Ok(format!("0x{}{}{}{}", hex::encode(ephemeral.as_bytes()), hex::encode(iv), hex::encode(tag), hex::encode(body)))
}

/// Try to open an encrypted note with our keys; None if it isn't ours (or doesn't match).
pub fn decrypt_note(data: &str, encryption_private: &str, public_key: U256, commitment: U256, index: usize) -> Option<Note> {
    let buf = hex::decode(data.trim_start_matches("0x")).ok()?;
    if buf.len() != 32 + 12 + 16 + 96 {
        return None;
    }
    let ephemeral = &buf[..32];
    let shared = x25519_secret(encryption_private).ok()?.diffie_hellman(&x25519_public(ephemeral).ok()?);
    let key = note_key(shared.as_bytes(), ephemeral);
    let mut ct = buf[60..].to_vec();
    ct.extend_from_slice(&buf[44..60]);
    let pt = Aes256Gcm::new_from_slice(&key).unwrap().decrypt(Nonce::from_slice(&buf[32..44]), Payload { msg: &ct, aad: &[] }).ok()?;
    let read = |i: usize| U256::from_be_slice(&pt[32 * i..32 * i + 32]);
    let note = Note { amount: read(0), blinding: read(1), asset: read(2), public_key, index: Some(index) };
    (commitment_of(&note) == commitment).then_some(note)
}
