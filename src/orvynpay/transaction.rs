//! Building and proving an OrvynPay private transaction (2 input notes, 2 output notes), exactly
//! as OrvynShieldedPool.transact checks it: extDataHash = keccak256(abi.encode(ExtData)) mod p,
//! publicAmount = extAmount - fee mod p, and the 9 public signals in circuit order.
//!
//! Proving needs the circuit's build output (contracts/circuits/build.sh): the witness generator
//! (transaction.wasm) and the proving key (transaction.zkey), at ORVYN_SHIELDED_WASM and
//! ORVYN_SHIELDED_ZKEY. The proof is Groth16 over BN254 with circom's QAP reduction, as snarkjs.

use std::path::PathBuf;

use alloy::primitives::{keccak256, Address, Bytes, I256, U256};

use alloy::sol_types::SolValue;

use super::crypto::{commitment_of, nullifier_of, random_field, MerkleTree, Note, FIELD};
use crate::error::{Error, Result};

pub const NOTE_LEVELS: usize = 20;
pub const KEY_LEVELS: usize = 16;

pub fn max_ext_amount() -> U256 {
    U256::from(1) << 248
}

pub use super::{ExtData, Proof};

pub fn ext_data_hash(e: &ExtData) -> U256 {
    // abi.encode of the struct as a single tuple argument.
    U256::from_be_bytes(keccak256((e.clone(),).abi_encode_params()).0) % FIELD
}

pub fn public_amount_of(ext_amount: I256, fee: U256) -> Result<U256> {
    let max = max_ext_amount();
    if fee >= max {
        return Err(Error::invalid("bad fee"));
    }
    let max_i = I256::from_raw(max);
    if ext_amount <= -max_i || ext_amount >= max_i {
        return Err(Error::invalid("bad amount"));
    }
    let amount = ext_amount - I256::from_raw(fee);
    Ok(if amount >= I256::ZERO { amount.into_raw() } else { FIELD - amount.unsigned_abs() })
}

pub struct BuildInput<'a> {
    pub private_key: U256,
    pub public_key: U256,
    /// 0 to 2 of our unspent notes (with their leaf index).
    pub inputs: Vec<Note>,
    /// 1 or 2 new notes (recipient and/or change). Zero-amount ones are allowed.
    pub outputs: Vec<Note>,
    /// Encrypted copies of the outputs, in order (each to its owner's encryption key).
    pub encrypted: [String; 2],
    /// > 0 deposit, < 0 withdrawal, 0 private transfer.
    pub ext_amount: I256,
    pub asset: U256,
    pub recipient: Option<Address>,
    pub relayer: Option<Address>,
    pub fee: U256,
    pub note_tree: &'a MerkleTree,
    pub key_tree: &'a MerkleTree,
}

#[derive(Debug, Clone)]
pub struct Signals {
    pub root: U256,
    pub key_root: U256,
    pub public_amount: U256,
    pub ext_data_hash: U256,
    pub asset: U256,
    pub input_nullifier: [U256; 2],
    pub output_commitment: [U256; 2],
}

#[derive(Debug, Clone)]
pub struct Built {
    /// Circuit input signals by name: each a flat list of field elements.
    pub circuit_input: Vec<(String, Vec<U256>)>,
    pub signals: Signals,
    pub ext: ExtData,
    pub outputs: Vec<Note>,
}

fn unhex(s: &str) -> Bytes {
    hex::decode(s.trim_start_matches("0x")).unwrap_or_default().into()
}

/// The circuit inputs and the contract's ExtData for a transaction; checks value is conserved.
pub fn build_transaction(t: BuildInput<'_>) -> Result<Built> {
    if t.inputs.len() > 2 || t.outputs.is_empty() || t.outputs.len() > 2 {
        return Err(Error::invalid("2 inputs and 2 outputs at most"));
    }
    let zero_note = |index: Option<usize>| Note { amount: U256::ZERO, public_key: t.public_key, blinding: random_field(), asset: t.asset, index };
    // Pad with zero-value notes (they need no membership proof, and random blindings keep their
    // nullifiers unique).
    let mut inputs = t.inputs.clone();
    while inputs.len() < 2 {
        inputs.push(zero_note(Some(0)));
    }
    let mut outputs = t.outputs.clone();
    while outputs.len() < 2 {
        outputs.push(zero_note(None));
    }
    if inputs.iter().chain(&outputs).any(|n| n.asset != t.asset) {
        return Err(Error::invalid("all notes must be in the transaction's asset"));
    }
    if inputs.iter().any(|n| n.amount > U256::ZERO && n.public_key != t.public_key) {
        return Err(Error::invalid("can only spend our own notes"));
    }
    let sum_in: U256 = inputs.iter().map(|n| n.amount).sum();
    let sum_out: U256 = outputs.iter().map(|n| n.amount).sum();
    let lhs = I256::from_raw(sum_in) + t.ext_amount - I256::from_raw(t.fee);
    if lhs != I256::from_raw(sum_out) {
        return Err(Error::invalid(format!("value isn't conserved: in {sum_in} + ext {} - fee {} != out {sum_out}", t.ext_amount, t.fee)));
    }
    if outputs.iter().any(|n| n.amount >= max_ext_amount()) {
        return Err(Error::invalid("output amount out of range"));
    }
    let ext = ExtData {
        recipient: t.recipient.unwrap_or(Address::ZERO),
        extAmount: t.ext_amount,
        relayer: t.relayer.unwrap_or(Address::ZERO),
        fee: t.fee,
        encryptedOutput1: unhex(&t.encrypted[0]),
        encryptedOutput2: unhex(&t.encrypted[1]),
    };
    let mut in_paths = vec![];
    for n in &inputs {
        if n.amount == U256::ZERO {
            in_paths.push((vec![U256::ZERO; NOTE_LEVELS], 0usize));
            continue;
        }
        let index = t.note_tree.index_of(commitment_of(n));
        if index.is_none() || index != n.index {
            return Err(Error::invalid("an input note isn't in the tree at its index"));
        }
        let i = index.unwrap();
        in_paths.push((t.note_tree.path(i)?, i));
    }
    let mut key_paths = vec![];
    for n in &outputs {
        if n.amount == U256::ZERO {
            key_paths.push((vec![U256::ZERO; KEY_LEVELS], 0usize));
            continue;
        }
        let i = t.key_tree.index_of(n.public_key).ok_or_else(|| Error::invalid("the recipient's shielded key isn't registered"))?;
        key_paths.push((t.key_tree.path(i)?, i));
    }
    let nullifiers = [
        nullifier_of(&Note { index: Some(in_paths[0].1), ..inputs[0].clone() }, t.private_key)?,
        nullifier_of(&Note { index: Some(in_paths[1].1), ..inputs[1].clone() }, t.private_key)?,
    ];
    let signals = Signals {
        root: t.note_tree.root(),
        key_root: t.key_tree.root(),
        public_amount: public_amount_of(t.ext_amount, t.fee)?,
        ext_data_hash: ext_data_hash(&ext),
        asset: t.asset,
        input_nullifier: nullifiers,
        output_commitment: [commitment_of(&outputs[0]), commitment_of(&outputs[1])],
    };
    let both = |f: &dyn Fn(usize) -> Vec<U256>| -> Vec<U256> { [f(0), f(1)].concat() };
    let circuit_input = vec![
        ("root".to_string(), vec![signals.root]),
        ("keyRoot".into(), vec![signals.key_root]),
        ("publicAmount".into(), vec![signals.public_amount]),
        ("extDataHash".into(), vec![signals.ext_data_hash]),
        ("asset".into(), vec![signals.asset]),
        ("inputNullifier".into(), signals.input_nullifier.to_vec()),
        ("outputCommitment".into(), signals.output_commitment.to_vec()),
        ("inAmount".into(), inputs.iter().map(|n| n.amount).collect()),
        ("inPrivateKey".into(), vec![t.private_key; 2]),
        ("inBlinding".into(), inputs.iter().map(|n| n.blinding).collect()),
        ("inPathIndices".into(), in_paths.iter().map(|p| U256::from(p.1)).collect()),
        ("inPathElements".into(), both(&|i| in_paths[i].0.clone())),
        ("outAmount".into(), outputs.iter().map(|n| n.amount).collect()),
        ("outPubkey".into(), outputs.iter().map(|n| n.public_key).collect()),
        ("outBlinding".into(), outputs.iter().map(|n| n.blinding).collect()),
        ("outKeyPathIndices".into(), key_paths.iter().map(|p| U256::from(p.1)).collect()),
        ("outKeyPathElements".into(), both(&|i| key_paths[i].0.clone())),
    ];
    Ok(Built { circuit_input, signals, ext, outputs })
}

pub fn artifacts() -> Result<(PathBuf, PathBuf)> {
    let wasm = PathBuf::from(std::env::var("ORVYN_SHIELDED_WASM").unwrap_or_else(|_| "contracts/circuits/build/transaction_js/transaction.wasm".into()));
    let zkey = PathBuf::from(std::env::var("ORVYN_SHIELDED_ZKEY").unwrap_or_else(|_| "contracts/circuits/build/transaction.zkey".into()));
    if !wasm.exists() || !zkey.exists() {
        return Err(Error::unavailable("OrvynPay private mode needs the circuit build (ORVYN_SHIELDED_WASM and ORVYN_SHIELDED_ZKEY)"));
    }
    Ok((wasm, zkey))
}

/// Prove a built transaction; returns the proof in OrvynShieldedPool's `Proof` shape. CPU-bound:
/// runs on a blocking thread.
pub async fn prove_transaction(built: &Built) -> Result<Proof> {
    let (wasm, zkey) = artifacts()?;
    let input = built.circuit_input.clone();
    let (a, b, c) = tokio::task::spawn_blocking(move || prover::prove(&wasm, &zkey, input)).await.map_err(|e| Error::internal(format!("prover: {e}")))??;
    let s = &built.signals;
    Ok(Proof {
        a,
        b,
        c,
        root: s.root,
        keyRoot: s.key_root,
        publicAmount: s.public_amount,
        extDataHash: s.ext_data_hash,
        asset: s.asset,
        inputNullifiers: s.input_nullifier,
        outputCommitments: s.output_commitment,
    })
}

/// The contract's public-signal order (OrvynShieldedPool._signals), for checking a proof.
pub fn signals_of(p: &Proof) -> Vec<U256> {
    vec![p.root, p.keyRoot, p.publicAmount, p.extDataHash, p.asset, p.inputNullifiers[0], p.inputNullifiers[1], p.outputCommitments[0], p.outputCommitments[1]]
}

/// Check a proof against the proving key's verifying key, as the pool's verifier would.
pub fn verify_proof(zkey: &std::path::Path, p: &Proof) -> Result<bool> {
    prover::verify(zkey, p.a, p.b, p.c, &signals_of(p))
}

mod prover {
    use std::fs::File;
    use std::path::Path;

    use alloy::primitives::U256;
    use ark_bn254::{Bn254, Fq, Fq2, Fr};
    use ark_circom::{read_zkey, CircomReduction, WitnessCalculator};
    use ark_ec::AffineRepr;
    use ark_ff::{BigInteger, PrimeField, UniformRand};
    use ark_groth16::Groth16;
    use num_bigint::BigInt;

    use crate::error::{Error, Result};

    fn fq(x: &Fq) -> U256 {
        U256::from_be_slice(&x.into_bigint().to_bytes_be())
    }
    fn fq2(x: &Fq2) -> [U256; 2] {
        // The verifier takes G2 coordinates as (c1, c0): the reverse of snarkjs' order.
        [fq(&x.c1), fq(&x.c0)]
    }

    type G1 = [U256; 2];
    type G2 = [[U256; 2]; 2];

    pub fn prove(wasm: &Path, zkey: &Path, input: Vec<(String, Vec<U256>)>) -> Result<(G1, G2, G1)> {
        let err = |what: &str, e: &dyn std::fmt::Display| Error::internal(format!("{what}: {e}"));
        let mut store = wasmer::Store::default();
        let mut calc = WitnessCalculator::new(&mut store, wasm).map_err(|e| err("witness generator", &e))?;
        let inputs = input.into_iter().map(|(name, values)| (name, values.into_iter().map(|v| BigInt::from_bytes_be(num_bigint::Sign::Plus, &v.to_be_bytes::<32>())).collect::<Vec<_>>()));
        let witness = calc.calculate_witness_element::<Fr, _>(&mut store, inputs, true).map_err(|e| err("witness", &e))?;
        let mut f = File::open(zkey).map_err(|e| err("proving key", &e))?;
        let (pk, m) = read_zkey(&mut f).map_err(|e| err("proving key", &e))?;
        let mut rng = ark_std::rand::rngs::OsRng;
        let (r, s) = (Fr::rand(&mut rng), Fr::rand(&mut rng));
        let proof = Groth16::<Bn254, CircomReduction>::create_proof_with_reduction_and_matrices(&pk, r, s, &[m.a, m.b, m.c], m.num_instance_variables, m.num_constraints, &witness)
            .map_err(|e| err("proof", &e))?;
        let (a, b, c) = (proof.a, proof.b, proof.c);
        let g1 = |p: &ark_bn254::G1Affine| -> G1 {
            let (x, y) = p.xy().expect("not the point at infinity");
            [fq(&x), fq(&y)]
        };
        let (bx, by) = b.xy().expect("not the point at infinity");
        Ok((g1(&a), [fq2(&bx), fq2(&by)], g1(&c)))
    }

    fn to_fq(x: U256) -> Fq {
        Fq::from_be_bytes_mod_order(&x.to_be_bytes::<32>())
    }

    pub fn verify(zkey: &Path, a: G1, b: G2, c: G1, signals: &[U256]) -> Result<bool> {
        use ark_bn254::{G1Affine, G2Affine};
        let err = |what: &str, e: &dyn std::fmt::Display| Error::internal(format!("{what}: {e}"));
        let mut f = File::open(zkey).map_err(|e| err("proving key", &e))?;
        let (pk, _) = read_zkey(&mut f).map_err(|e| err("proving key", &e))?;
        let pvk = ark_groth16::prepare_verifying_key(&pk.vk);
        // G2 coordinates come in the verifier's (c1, c0) order.
        let g2 = |x: [U256; 2]| Fq2::new(to_fq(x[1]), to_fq(x[0]));
        let proof = ark_groth16::Proof::<Bn254> {
            a: G1Affine::new(to_fq(a[0]), to_fq(a[1])),
            b: G2Affine::new(g2(b[0]), g2(b[1])),
            c: G1Affine::new(to_fq(c[0]), to_fq(c[1])),
        };
        let inputs: Vec<Fr> = signals.iter().map(|s| Fr::from_be_bytes_mod_order(&s.to_be_bytes::<32>())).collect();
        Groth16::<Bn254, CircomReduction>::verify_proof(&pvk, &proof, &inputs).map_err(|e| err("verify", &e))
    }
}
