//! The guard in front of every agent wallet. Privy holds the keys and signs; nothing reaches Privy
//! until this has checked it:
//!
//! - Agents Orvyn has held (agents.held_reason) can't sign anything.
//! - At most `rate_per_min` signatures per agent per minute (per process).
//! - What the signature does must pass the rules, and what it spends must fit the daily caps.
//! - Transfers out need the owner's signed withdrawal (grants.rs), used once.
//! - Raw hash signing and EIP-7702 authorizations aren't offered at all.
//!
//! Every request, signed or refused, is written to wallet_log.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::aster::{self, MarkPrice};
use super::config::cfg;
use super::grants::OwnerGrant;
use super::rules::{check_evm_message, check_evm_transaction, check_typed_data};
use super::spend::reserve_spends;
use super::{refuse, Checked, EvmAgent, TxRequest, TypedData};
use crate::db::db;
use crate::error::{Error, Kind, Result};

/// Something that holds a wallet's key and signs: Privy, or a local key (self-hosted agents).
/// Signatures and signed transactions are 0x hex.
#[async_trait]
pub trait RawSigner: Send + Sync {
    fn address(&self) -> String;
    async fn sign_message(&self, message: &[u8]) -> Result<String>;
    async fn sign_typed_data(&self, typed: &TypedData) -> Result<String>;
    async fn sign_transaction(&self, tx: &TxRequest) -> Result<String>;
}

static RECENT: LazyLock<Mutex<HashMap<String, Vec<Instant>>>> = LazyLock::new(Default::default);

fn rate_limit(agent_id: &str) -> Result<()> {
    let limit = cfg().rate_per_min;
    let mut recent = RECENT.lock().unwrap();
    let times = recent.entry(agent_id.to_string()).or_default();
    times.retain(|t| t.elapsed() < Duration::from_secs(60));
    if times.len() >= limit {
        return Err(refuse(format!("rate limit: {limit} signatures per minute")));
    }
    times.push(Instant::now());
    Ok(())
}

/// The agent's Aster API wallet address, kept in the Aster plugin's state.
pub async fn aster_signer_of(agent_id: &str) -> Result<Option<String>> {
    let state: Option<Value> = sqlx::query_scalar("select state from agent_plugins where agent_id = $1 and plugin_id = 'aster'").bind(agent_id).fetch_optional(db()).await?;
    Ok(state.and_then(|s| s.pointer("/asterSigner/address").and_then(Value::as_str).map(str::to_lowercase)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Main,
    Aster,
}

/// The agent as the rules see it, for a request from `address`. A main wallet can exist before its
/// agent row (during creation).
async fn agent_of(agent_id: &str, address: &str, role: Role) -> Result<EvmAgent> {
    let row: Option<(String, Option<String>, String)> =
        sqlx::query_as("select owner_account_id, held_reason, evm_address from agents where id = $1").bind(agent_id).fetch_optional(db()).await?;
    if let Some((_, Some(reason), _)) = &row {
        return Err(refuse(format!("agent is paused by Orvyn: {reason}")));
    }
    let aster_signer = if row.is_some() { aster_signer_of(agent_id).await? } else { None };
    let address = address.to_lowercase();
    if role == Role::Aster {
        let ok = row.is_some() && aster_signer.as_deref() == Some(address.as_str());
        let Some((owner, _, evm)) = row.filter(|_| ok) else { return Err(refuse("this isn't the agent's Aster API wallet")) };
        return Ok(EvmAgent { agent_id: agent_id.into(), address: evm.to_lowercase(), owner: owner.to_lowercase(), aster_signer });
    }
    if let Some((_, _, evm)) = &row {
        if evm.to_lowercase() != address {
            return Err(refuse("this wallet isn't the agent's"));
        }
    }
    // No row yet: no owner, so no withdrawals can be authorized.
    let owner = row.map(|(o, _, _)| o.to_lowercase()).unwrap_or_else(|| "0x0000000000000000000000000000000000000000".into());
    Ok(EvmAgent { agent_id: agent_id.into(), address, owner, aster_signer })
}

async fn log(agent_id: &str, scheme: &str, ok: bool, summary: Option<&str>, error: Option<&str>) {
    let error = error.map(|e| e.chars().take(500).collect::<String>());
    let r = sqlx::query("insert into wallet_log (agent_id, scheme, ok, summary, error) values ($1, $2, $3, $4, $5)")
        .bind(agent_id)
        .bind(scheme)
        .bind(ok)
        .bind(summary)
        .bind(error)
        .execute(db())
        .await;
    if let Err(e) = r {
        eprintln!("[wallet-policy] log: {e}");
    }
}

/// Check and record one signature, then sign. Refusals and cap overruns become 403s prefixed
/// "wallet policy:"; the signer's own errors pass through as they are.
async fn authorize<C, CF, S, SF>(agent_id: &str, address: &str, scheme: &str, role: Role, check: C, sign: S) -> Result<String>
where
    C: FnOnce(EvmAgent) -> CF,
    CF: Future<Output = Result<Checked>>,
    S: FnOnce() -> SF,
    SF: Future<Output = Result<String>>,
{
    let mut summary: Option<String> = None;
    let checked = async {
        rate_limit(agent_id)?;
        let agent = agent_of(agent_id, address, role).await?;
        let result = check(agent).await?;
        summary = Some(result.summary.clone());
        reserve_spends(agent_id, &result.spends).await?;
        if let Some(nonce) = &result.grant_nonce {
            let used = sqlx::query("insert into wallet_grants_used (nonce, agent_id) values ($1, $2) on conflict do nothing").bind(nonce).bind(agent_id).execute(db()).await?;
            if used.rows_affected() == 0 {
                return Err(refuse("this withdrawal grant was already used"));
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = checked {
        log(agent_id, scheme, false, summary.as_deref(), Some(&e.message)).await;
        return Err(if e.kind == Kind::Forbidden { Error::forbidden(format!("wallet policy: {}", e.message)) } else { e });
    }
    match sign().await {
        Ok(out) => {
            log(agent_id, scheme, true, summary.as_deref(), None).await;
            Ok(out)
        }
        Err(e) => {
            log(agent_id, scheme, false, summary.as_deref(), Some(&format!("signing failed: {}", e.message))).await;
            Err(e)
        }
    }
}

/// An agent's main wallet behind the policy. Every signature is checked first.
pub struct GuardedWallet {
    pub agent_id: String,
    inner: Box<dyn RawSigner>,
    /// The owner's signed withdrawal, for the one transfer it authorizes.
    grant: Option<OwnerGrant>,
}

impl GuardedWallet {
    pub fn new(agent_id: &str, inner: Box<dyn RawSigner>) -> Self {
        Self { agent_id: agent_id.into(), inner, grant: None }
    }
    /// Attach the owner's signed withdrawal (its transfer can then be signed, once).
    pub fn with_grant(mut self, grant: OwnerGrant) -> Self {
        self.grant = Some(grant);
        self
    }
    pub fn address(&self) -> String {
        self.inner.address()
    }

    pub async fn sign_message(&self, message: &[u8]) -> Result<String> {
        let raw = message.to_vec();
        authorize(
            &self.agent_id,
            &self.address(),
            "evm-message",
            Role::Main,
            |_| async move { Ok(Checked::new(check_evm_message(&raw)?, Default::default())) },
            || self.inner.sign_message(message),
        )
        .await
    }

    pub async fn sign_typed_data(&self, typed: &TypedData) -> Result<String> {
        authorize(&self.agent_id, &self.address(), "evm-typed-data", Role::Main, |agent| async move { check_typed_data(typed, &agent) }, || self.inner.sign_typed_data(typed)).await
    }

    pub async fn sign_transaction(&self, tx: &TxRequest) -> Result<String> {
        let grant = self.grant.as_ref();
        authorize(
            &self.agent_id,
            &self.address(),
            "evm-transaction",
            Role::Main,
            |agent| async move {
                let mut c = check_evm_transaction(tx, &agent, grant).await?;
                c.summary = format!("chain {}: {}", tx.chain_id, c.summary);
                Ok(c)
            },
            || self.inner.sign_transaction(tx),
        )
        .await
    }
}

/// The agent's Aster API wallet behind the Aster rules: it only signs Aster API requests for the
/// agent's own account. No messages, no transactions.
pub struct GuardedAsterWallet {
    pub agent_id: String,
    inner: Box<dyn RawSigner>,
    mark_price: MarkPrice,
}

impl GuardedAsterWallet {
    pub fn new(agent_id: &str, inner: Box<dyn RawSigner>) -> Self {
        Self::with_mark_price(agent_id, inner, aster::aster_mark_price())
    }
    pub fn with_mark_price(agent_id: &str, inner: Box<dyn RawSigner>, mark_price: MarkPrice) -> Self {
        Self { agent_id: agent_id.into(), inner, mark_price }
    }
    pub fn address(&self) -> String {
        self.inner.address()
    }

    async fn refuse_other(&self) -> Result<String> {
        log(&self.agent_id, "aster-request", false, None, Some("the Aster API wallet only signs Aster requests")).await;
        Err(Error::forbidden("wallet policy: the Aster API wallet only signs Aster requests"))
    }
    pub async fn sign_message(&self, _message: &[u8]) -> Result<String> {
        self.refuse_other().await
    }
    pub async fn sign_transaction(&self, _tx: &TxRequest) -> Result<String> {
        self.refuse_other().await
    }

    pub async fn sign_typed_data(&self, typed: &TypedData) -> Result<String> {
        let address = self.address();
        let mark = &self.mark_price;
        authorize(
            &self.agent_id,
            &address,
            "aster-request",
            Role::Aster,
            |agent| {
                let address = address.clone();
                async move { aster::check_aster_signer_typed_data(typed, &agent, &address, mark).await }
            },
            || self.inner.sign_typed_data(typed),
        )
        .await
    }
}
