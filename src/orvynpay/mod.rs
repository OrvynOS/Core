//! OrvynPay: agent-to-agent payments on BNB Chain in USDT, USDC or BNB, between agents that hold a
//! Binance Agent ID (both sides). Payees are addressed by their Agent ID (`name.ai`), their Orvyn
//! handle, or a 0x address.
//!
//! - Standard mode (OrvynPay.sol): a public payment with a reference.
//! - Private mode (OrvynShieldedPool.sol): deposit into the shielded pool, pay another agent inside
//!   it (amounts and parties hidden on chain), and withdraw. The agent's shielded keys are sealed in
//!   the database (shielded_keys); its notes are found by scanning the pool's events.

pub mod crypto;
pub mod transaction;

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use alloy::primitives::{Address, Bytes, FixedBytes, I256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent as _};
use serde_json::json;

pub use crypto::*;
pub use transaction::*;

use crate::agentid::{agent_id_label, agent_id_owner, holds_agent_id};
use crate::billing::Call;
use crate::db::{db, ShieldedKey};
use crate::env::env;
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};
use crate::secrets::{decrypt_secret, encrypt_secret, SecretKind};

pub const NATIVE: &str = "0x0000000000000000000000000000000000000000";

/// (symbol, address, decimals).
pub const TOKENS: &[(&str, &str, u8)] = &[("BNB", NATIVE, 18), ("USDT", "0x55d398326f99059fF775485246999027B3197955", 18), ("USDC", "0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d", 18)];

pub fn token(symbol: &str) -> Result<(&'static str, u8)> {
    TOKENS.iter().find(|t| t.0 == symbol).map(|t| (t.1, t.2)).ok_or_else(|| Error::invalid(format!("token must be one of {}", TOKENS.iter().map(|t| t.0).collect::<Vec<_>>().join(", "))))
}

sol! {
    #[derive(Debug, PartialEq)]
    struct ExtData { address recipient; int256 extAmount; address relayer; uint256 fee; bytes encryptedOutput1; bytes encryptedOutput2; }
    #[derive(Debug, PartialEq)]
    struct Proof { uint256[2] a; uint256[2][2] b; uint256[2] c; uint256 root; uint256 keyRoot; uint256 publicAmount; uint256 extDataHash; uint256 asset; uint256[2] inputNullifiers; uint256[2] outputCommitments; }
    #[sol(rpc)]
    interface OrvynPay {
        function pay(address to, address token, uint256 amount, bytes32 ref) payable;
        function hasAgentId(address account) view returns (bool);
        event Paid(address indexed from, address indexed to, address indexed token, uint256 amount, bytes32 ref, bool signed);
    }
    #[sol(rpc)]
    interface ShieldedPool {
        function registerKey(uint256 shieldedKey, bytes encryptionKey);
        function transact(Proof p, ExtData e) payable;
        function shieldedKeyOf(address account) view returns (uint256);
        function nullifierSpent(uint256 nullifier) view returns (bool);
        function isKnownNoteRoot(uint256 root) view returns (bool);
        function isKnownKeyRoot(uint256 root) view returns (bool);
        event NewCommitment(uint256 commitment, uint256 index, bytes encryptedOutput);
        event NewNullifier(uint256 nullifier);
        event KeyRegistered(address indexed account, uint256 shieldedKey, bytes encryptionKey, uint256 index);
    }
}

fn pay_error(m: impl Into<String>) -> Error {
    Error::invalid(m)
}

fn call(to: Address, data: Vec<u8>, value: U256) -> Call {
    Call { to: to.to_string(), data: format!("0x{}", hex::encode(data)), value: value.to_string() }
}

// ---- Payees ----

#[derive(Debug, Clone, serde::Serialize)]
pub struct Payee {
    pub address: Address,
    /// How it was named: "alice.ai", "alice (Orvyn)" or the address.
    pub label: String,
}

fn root_domain() -> String {
    std::env::var("ORVYN_ROOT_DOMAIN").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "orvyn.cash".into())
}

/// Resolve who to pay: `name.ai` (the wallet holding that Binance Agent ID), an Orvyn agent's
/// handle (`alice` or `alice.orvyn.cash`), or a 0x address. The payee must hold an Agent ID.
pub async fn resolve_payee(input: &str) -> Result<Payee> {
    let raw = input.trim();
    let payee = if evm::is_address(raw) {
        let a = evm::parse_address(raw)?;
        Payee { address: a, label: a.to_string() }
    } else if raw.to_lowercase().ends_with(".ai") {
        let label = agent_id_label(raw).ok_or_else(|| pay_error(format!("{raw} isn't a valid Agent ID")))?;
        let owner = agent_id_owner(&label).await?.ok_or_else(|| pay_error(format!("{label}.ai isn't registered")))?;
        Payee { address: owner, label: format!("{label}.ai") }
    } else {
        let lower = raw.to_lowercase();
        let suffix = format!(".{}", root_domain());
        let handle = lower.strip_suffix(&suffix).unwrap_or(&lower);
        let row: Option<(String, Option<String>)> = sqlx::query_as("select evm_address, handle from agents where handle = $1 or agent_id_name = $1 limit 1").bind(handle).fetch_optional(db()).await?;
        let (address, h) = row.ok_or_else(|| pay_error(format!("no agent called {raw}; use name.ai, an Orvyn handle or a 0x address")))?;
        Payee { address: evm::parse_address(&address)?, label: format!("{} (Orvyn)", h.unwrap_or_default()) }
    };
    if !holds_agent_id(&payee.address.to_string()).await {
        return Err(pay_error(format!("{} doesn't hold a Binance Agent ID, so it can't receive OrvynPay payments", payee.label)));
    }
    Ok(payee)
}

// ---- Standard mode ----

/// A payment reference: its first 32 bytes, right-padded.
pub fn ref_of(r: &str) -> FixedBytes<32> {
    let bytes: Vec<u8> = r.chars().take(32).collect::<String>().into_bytes();
    let mut out = [0u8; 32];
    let n = bytes.len().min(32);
    out[..n].copy_from_slice(&bytes[..n]);
    FixedBytes(out)
}

/// The calls for a standard payment: an exact approval (tokens) and `pay`.
pub fn standard_payment_calls(to: Address, symbol: &str, amount: U256, reference: &str) -> Result<Vec<Call>> {
    let contract: Address = env().orvyn_pay_contract.as_deref().and_then(|a| a.parse().ok()).ok_or_else(|| pay_error("OrvynPay isn't deployed on this instance (ORVYN_PAY_CONTRACT)"))?;
    let (t, _) = token(symbol)?;
    let t = evm::parse_address(t)?;
    let native = t == Address::ZERO;
    let pay = call(contract, OrvynPay::payCall { to, token: t, amount, r#ref: ref_of(reference) }.abi_encode(), if native { amount } else { U256::ZERO });
    if native {
        return Ok(vec![pay]);
    }
    Ok(vec![call(t, evm::IERC20::approveCall { spender: contract, amount }.abi_encode(), U256::ZERO), pay])
}

// ---- Private mode: keys ----

#[derive(Debug, Clone)]
pub struct AgentShieldedKeys {
    pub keys: ShieldedKeys,
    pub registered_tx: Option<String>,
}

/// The agent's shielded keys, created (and sealed in the database) on first use.
pub async fn shielded_keys_of(agent_id: &str) -> Result<AgentShieldedKeys> {
    for _ in 0..2 {
        let row: Option<ShieldedKey> = sqlx::query_as("select * from shielded_keys where agent_id = $1").bind(agent_id).fetch_optional(db()).await?;
        if let Some(row) = row {
            let s: serde_json::Value = serde_json::from_str(&decrypt_secret(&row.sealed, agent_id, SecretKind::ShieldedKey)?).map_err(|_| Error::internal("bad sealed keys"))?;
            let pk: U256 = s["privateKey"].as_str().and_then(|x| x.parse().ok()).ok_or_else(|| Error::internal("bad sealed keys"))?;
            return Ok(AgentShieldedKeys {
                keys: ShieldedKeys {
                    private_key: pk,
                    public_key: row.public_key.parse().map_err(|_| Error::internal("bad public key"))?,
                    encryption_private: s["encryptionPrivate"].as_str().unwrap_or("").into(),
                    encryption_public: row.encryption_public,
                },
                registered_tx: row.registered_tx,
            });
        }
        let k = new_shielded_keys();
        let sealed = encrypt_secret(&json!({ "privateKey": k.private_key.to_string(), "encryptionPrivate": k.encryption_private }).to_string(), agent_id, SecretKind::ShieldedKey)?;
        sqlx::query("insert into shielded_keys (agent_id, public_key, encryption_public, sealed) values ($1, $2, $3, $4) on conflict do nothing")
            .bind(agent_id)
            .bind(k.public_key.to_string())
            .bind(&k.encryption_public)
            .bind(sealed)
            .execute(db())
            .await?;
        // Read back: the winner of a race.
    }
    Err(Error::internal("couldn't create shielded keys"))
}

pub async fn mark_registered(agent_id: &str, tx: &str) -> Result<()> {
    sqlx::query("update shielded_keys set registered_tx = $2 where agent_id = $1").bind(agent_id).bind(tx).execute(db()).await?;
    Ok(())
}

pub fn register_key_call(pool: Address, keys: &ShieldedKeys) -> Call {
    let enc: Bytes = hex::decode(keys.encryption_public.trim_start_matches("0x")).unwrap_or_default().into();
    call(pool, ShieldedPool::registerKeyCall { shieldedKey: keys.public_key, encryptionKey: enc }.abi_encode(), U256::ZERO)
}

// ---- Private mode: the pool's state ----

#[derive(Debug, Clone)]
pub struct RegisteredKey {
    pub shielded_key: U256,
    pub encryption_key: String,
}

#[derive(Debug, Clone)]
pub struct PoolView {
    pub note_tree: MerkleTree,
    pub key_tree: MerkleTree,
    /// Encrypted outputs by leaf index.
    pub encrypted: HashMap<usize, (U256, String)>,
    pub spent: HashSet<U256>,
    /// Registered keys by account (lowercase).
    pub keys_by_account: HashMap<String, RegisteredKey>,
    pub to_block: u64,
}

static CACHE: LazyLock<Mutex<HashMap<Address, PoolView>>> = LazyLock::new(Default::default);
const CHUNK: u64 = 5_000;

/// Read the pool's events (incrementally, cached per process) into its two trees.
pub async fn pool_view(pool: Address) -> Result<PoolView> {
    let p = evm::bsc();
    let cached = CACHE.lock().unwrap().get(&pool).cloned();
    let head = p.get_block_number().await.map_err(rpc_error)?;
    let start = match &cached {
        Some(v) => v.to_block + 1,
        None => std::env::var("ORVYN_SHIELDED_POOL_FROM_BLOCK").ok().and_then(|b| b.parse().ok()).unwrap_or(0),
    };
    let mut view = cached.unwrap_or_else(|| PoolView {
        note_tree: MerkleTree::new(NOTE_LEVELS, vec![]),
        key_tree: MerkleTree::new(KEY_LEVELS, vec![]),
        encrypted: HashMap::new(),
        spent: HashSet::new(),
        keys_by_account: HashMap::new(),
        to_block: start.saturating_sub(1),
    });
    let mut from = start;
    while from <= head {
        let to = (from + CHUNK - 1).min(head);
        let filter = |sig| Filter::new().address(pool).event_signature(sig).from_block(from).to_block(to);
        let (commitments, nullifiers, keys) = (
            p.get_logs(&filter(ShieldedPool::NewCommitment::SIGNATURE_HASH)).await.map_err(rpc_error)?,
            p.get_logs(&filter(ShieldedPool::NewNullifier::SIGNATURE_HASH)).await.map_err(rpc_error)?,
            p.get_logs(&filter(ShieldedPool::KeyRegistered::SIGNATURE_HASH)).await.map_err(rpc_error)?,
        );
        let mut leaves: Vec<(usize, U256, String)> = commitments
            .iter()
            .filter_map(|l| ShieldedPool::NewCommitment::decode_log_data(l.data()).ok())
            .map(|e| (e.index.to::<u64>() as usize, e.commitment, format!("0x{}", hex::encode(&e.encryptedOutput))))
            .collect();
        leaves.sort_by_key(|l| l.0);
        let mut new_leaves = vec![];
        for (index, commitment, data) in leaves {
            if index != view.note_tree.leaves().len() + new_leaves.len() {
                return Err(Error::internal("missing pool events; rescan from the deployment block"));
            }
            new_leaves.push(commitment);
            view.encrypted.insert(index, (commitment, data));
        }
        if !new_leaves.is_empty() {
            view.note_tree.insert(&new_leaves);
        }
        for l in &nullifiers {
            if let Ok(e) = ShieldedPool::NewNullifier::decode_log_data(l.data()) {
                view.spent.insert(e.nullifier);
            }
        }
        let mut regs: Vec<ShieldedPool::KeyRegistered> = keys.iter().filter_map(|l| ShieldedPool::KeyRegistered::decode_log_data(l.data()).ok()).collect();
        regs.sort_by_key(|k| k.index);
        if !regs.is_empty() {
            view.key_tree.insert(&regs.iter().map(|k| k.shieldedKey).collect::<Vec<_>>());
        }
        for k in regs {
            view.keys_by_account.insert(format!("{:#x}", k.account), RegisteredKey { shielded_key: k.shieldedKey, encryption_key: format!("0x{}", hex::encode(&k.encryptionKey)) });
        }
        view.to_block = to;
        from = to + 1;
    }
    CACHE.lock().unwrap().insert(pool, view.clone());
    Ok(view)
}

/// Our unspent notes in the pool.
pub fn unspent_notes(view: &PoolView, keys: &ShieldedKeys) -> Vec<Note> {
    let mut indices: Vec<&usize> = view.encrypted.keys().collect();
    indices.sort();
    let mut notes = vec![];
    for i in indices {
        let (commitment, data) = &view.encrypted[i];
        if let Some(n) = decrypt_note(data, &keys.encryption_private, keys.public_key, *commitment, *i) {
            if n.amount > U256::ZERO && nullifier_of(&n, keys.private_key).is_ok_and(|x| !view.spent.contains(&x)) {
                notes.push(n);
            }
        }
    }
    notes
}

pub fn shielded_balance(notes: &[Note], asset: U256) -> U256 {
    notes.iter().filter(|n| n.asset == asset).map(|n| n.amount).sum()
}

pub fn asset_of(symbol: &str) -> Result<U256> {
    Ok(U256::from_be_slice(evm::parse_address(token(symbol)?.0)?.as_slice()))
}

/// Pick up to two notes covering `amount` (largest first).
fn pick(notes: &[Note], asset: U256, amount: U256) -> Result<(Vec<Note>, U256)> {
    let mut mine: Vec<Note> = notes.iter().filter(|n| n.asset == asset).cloned().collect();
    mine.sort_by(|a, b| b.amount.cmp(&a.amount));
    let chosen: Vec<Note> = mine.iter().take(2).cloned().collect();
    let total: U256 = chosen.iter().map(|n| n.amount).sum();
    if total < amount {
        return Err(pay_error(if shielded_balance(&mine, asset) >= amount {
            "the shielded balance is split across too many notes; merge them first (a payment to yourself)"
        } else {
            "not enough shielded balance"
        }));
    }
    Ok((chosen, total - amount))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PrivateKind {
    Deposit,
    Transfer,
    Withdraw,
}

pub struct PrivateTx {
    pub proof: Proof,
    pub ext: ExtData,
    /// BNB to attach (deposits of BNB).
    pub value: U256,
}

/// Build (not prove) a private transaction for the agent:
/// - deposit: `amount` from its wallet into a note of its own;
/// - transfer: `amount` to `to` (a registered account) inside the pool, change back to itself;
/// - withdraw: `amount` out of the pool to its own wallet.
pub fn build_private(kind: PrivateKind, keys: &ShieldedKeys, view: &PoolView, symbol: &str, amount: U256, self_address: Address, to: Option<Address>) -> Result<Built> {
    let asset = asset_of(symbol)?;
    let mine = view.keys_by_account.get(&format!("{self_address:#x}"));
    if mine.is_none_or(|m| m.shielded_key != keys.public_key) {
        return Err(pay_error("the agent's shielded key isn't registered in the pool yet"));
    }
    let note = |amount: U256, owner: U256| Note { amount, public_key: owner, blinding: random_field(), asset, index: None };
    let mut inputs: Vec<Note> = vec![];
    let mut ext_amount = I256::ZERO;
    let mut recipient: Option<Address> = None;
    let (outputs, encrypted) = match kind {
        PrivateKind::Deposit => {
            let out = [note(amount, keys.public_key), note(U256::ZERO, keys.public_key)];
            ext_amount = I256::from_raw(amount);
            let enc = [encrypt_note(&out[0], &keys.encryption_public)?, encrypt_note(&out[1], &keys.encryption_public)?];
            (out.to_vec(), enc)
        }
        _ => {
            let (picked, change) = pick(&unspent_notes(view, keys), asset, amount)?;
            inputs = picked;
            let change = note(change, keys.public_key);
            if kind == PrivateKind::Transfer {
                let payee = to.and_then(|t| view.keys_by_account.get(&format!("{t:#x}"))).ok_or_else(|| pay_error("the payee hasn't registered a shielded key, so it can't be paid privately"))?;
                let out = note(amount, payee.shielded_key);
                let enc = [encrypt_note(&out, &payee.encryption_key)?, encrypt_note(&change, &keys.encryption_public)?];
                (vec![out, change], enc)
            } else {
                let zero = note(U256::ZERO, keys.public_key);
                let enc = [encrypt_note(&change, &keys.encryption_public)?, encrypt_note(&zero, &keys.encryption_public)?];
                ext_amount = -I256::from_raw(amount);
                recipient = Some(self_address);
                (vec![change, zero], enc)
            }
        }
    };
    for n in &inputs {
        if view.note_tree.index_of(commitment_of(n)) != n.index {
            return Err(pay_error("note index mismatch; rescan the pool"));
        }
    }
    build_transaction(BuildInput {
        private_key: keys.private_key,
        public_key: keys.public_key,
        inputs,
        outputs,
        encrypted,
        ext_amount,
        asset,
        recipient,
        relayer: None,
        fee: U256::ZERO,
        note_tree: &view.note_tree,
        key_tree: &view.key_tree,
    })
}

/// Build and prove a private transaction for the agent.
pub async fn private_transaction(kind: PrivateKind, keys: &ShieldedKeys, view: &PoolView, symbol: &str, amount: U256, self_address: Address, to: Option<Address>) -> Result<PrivateTx> {
    let built = build_private(kind, keys, view, symbol, amount, self_address, to)?;
    let proof = prove_transaction(&built).await?;
    let value = if kind == PrivateKind::Deposit && symbol == "BNB" { amount } else { U256::ZERO };
    Ok(PrivateTx { proof, ext: built.ext, value })
}

/// The calls to submit a private transaction (an exact approval first for token deposits).
pub fn private_transaction_calls(pool: Address, tx: &PrivateTx, symbol: &str) -> Result<Vec<Call>> {
    let transact = call(pool, ShieldedPool::transactCall { p: tx.proof.clone(), e: tx.ext.clone() }.abi_encode(), tx.value);
    if tx.ext.extAmount > I256::ZERO && symbol != "BNB" {
        let t = evm::parse_address(token(symbol)?.0)?;
        return Ok(vec![call(t, evm::IERC20::approveCall { spender: pool, amount: tx.ext.extAmount.into_raw() }.abi_encode(), U256::ZERO), transact]);
    }
    Ok(vec![transact])
}
