//! OrvynID: an agent's verifiable identity on BNB Chain.
//!
//! - Proofs: the agent's wallet signs (EIP-191) a short-lived claim naming who it's talking to
//!   (`audience`); anyone verifies it against the agent's wallet.
//! - Identity document: what Orvyn attests about the agent, signed with ORVYN_ATTESTATION_KEY.
//! The claim's JSON is the TypeScript core's field for field (it's what gets signed).

use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agentid::{agent_id_label, is_premium};
use crate::db::{db, Agent};
use crate::env::env;
use crate::error::{Error, Result};
use crate::public::{iso, public_status};

const MAX_TTL: i64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u8,
    pub agent: String,
    pub wallet: String,
    pub agent_id: Option<String>,
    pub audience: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrvynIdProof {
    pub claim: Claim,
    pub signature: String,
}

/// What gets signed: the claim's JSON, keys in the TypeScript core's order.
pub fn claim_message(c: &Claim) -> String {
    serde_json::to_string(c).unwrap()
}

/// A proof that the agent controls its wallet, for `audience` (a host name or URL).
pub async fn create_proof(agent: &Agent, audience: &str, ttl_seconds: Option<i64>) -> Result<OrvynIdProof> {
    let handle = agent.handle.clone().ok_or_else(|| Error::invalid("The agent has no handle yet."))?;
    let audience = audience.trim().to_lowercase();
    if audience.is_empty() || audience.len() > 253 {
        return Err(Error::invalid("audience must be a host name or URL."));
    }
    let ttl = ttl_seconds.unwrap_or(600).clamp(30, MAX_TTL);
    let now = chrono::Utc::now().timestamp();
    let claim = Claim {
        kind: "orvyn-id-proof".into(),
        version: 1,
        agent: handle,
        wallet: agent.evm_address.clone(),
        agent_id: is_premium(agent).then(|| format!("{}.ai", agent.agent_id_name.clone().unwrap_or_default())),
        audience,
        issued_at: now,
        expires_at: now + ttl,
        nonce: hex::encode(rand::random::<[u8; 12]>()),
    };
    let wallet = crate::wallets::agent_wallet(&agent.id, &agent.evm_wallet)?;
    let signature = wallet.sign_message(claim_message(&claim).as_bytes()).await?;
    Ok(OrvynIdProof { claim, signature })
}

pub fn proof_header(p: &OrvynIdProof) -> String {
    B64URL.encode(serde_json::to_vec(p).unwrap())
}

pub fn parse_proof_header(header: &str) -> Result<OrvynIdProof> {
    let bytes = B64URL.decode(header.trim_end_matches('=')).map_err(|_| Error::invalid("Malformed proof."))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::invalid("Malformed proof."))
}

/// Check a proof: fresh, for `audience`, signed by the wallet of a live agent. Ok(Err(reason)) when
/// it isn't valid.
pub async fn verify_proof(p: &OrvynIdProof, audience: Option<&str>) -> Result<std::result::Result<Value, &'static str>> {
    let c = &p.claim;
    if c.kind != "orvyn-id-proof" || c.version != 1 {
        return Ok(Err("malformed proof"));
    }
    let now = chrono::Utc::now().timestamp();
    if c.expires_at < now || c.issued_at > now + 60 || c.expires_at - c.issued_at > MAX_TTL {
        return Ok(Err("expired"));
    }
    if audience.is_some_and(|a| c.audience != a.trim().to_lowercase()) {
        return Ok(Err("wrong audience"));
    }
    let sig = hex::decode(p.signature.trim_start_matches("0x")).unwrap_or_default();
    let Some(signer) = crate::evm::sig::recover(crate::evm::sig::message_hash(&claim_message(c)), &sig) else { return Ok(Err("bad signature")) };
    let signer = format!("{signer:#x}");
    let agent = find_agent_by_identifier(&c.agent).await?;
    match agent {
        Some(a) if a.evm_address.eq_ignore_ascii_case(&signer) && c.wallet.eq_ignore_ascii_case(&signer) => Ok(Ok(identity_document(&a).await?)),
        _ => Ok(Err("not signed by this agent's wallet")),
    }
}

/// An agent by handle, id, wallet address, or Binance Agent ID (name.ai). Held agents are hidden.
pub async fn find_agent_by_identifier(identifier: &str) -> Result<Option<Agent>> {
    let raw = identifier.trim().to_lowercase();
    if raw.is_empty() {
        return Ok(None);
    }
    let root = std::env::var("ORVYN_ROOT_DOMAIN").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "orvyn.cash".into());
    let handle = raw.strip_suffix(".orvyn.cash").or_else(|| raw.strip_suffix(&format!(".{root}"))).unwrap_or(&raw).to_string();
    let row: Option<Agent> = if raw.ends_with(".ai") {
        match agent_id_label(&raw) {
            Some(label) => sqlx::query_as("select * from agents where agent_id_name = $1").bind(label).fetch_optional(db()).await?,
            None => None,
        }
    } else if crate::evm::is_address(&raw) {
        sqlx::query_as("select * from agents where evm_address = $1").bind(&raw).fetch_optional(db()).await?
    } else {
        sqlx::query_as("select * from agents where handle = $1 or id = $2 limit 1").bind(&handle).bind(&raw).fetch_optional(db()).await?
    };
    Ok(row.filter(|a| a.held_reason.is_none()))
}

/// The address Orvyn signs identity documents with, published for verifiers.
pub fn attestation_address() -> Option<String> {
    let key = env().orvyn_attestation_key.as_deref()?;
    key.parse::<PrivateKeySigner>().ok().map(|s| s.address().to_string())
}

/// What Orvyn attests about the agent, signed with its attestation key when one is configured.
pub async fn identity_document(agent: &Agent) -> Result<Value> {
    let e = env();
    let root = std::env::var("ORVYN_ROOT_DOMAIN").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "orvyn.cash".into()).to_lowercase();
    let premium = is_premium(agent);
    let document = json!({
        "type": "orvyn-id",
        "version": 1,
        "handle": agent.handle,
        "url": agent.handle.as_ref().map(|h| format!("https://{h}.{root}")),
        "email": agent.handle.as_ref().map(|h| format!("{h}@{}", e.orvyn_mail_domain)),
        "name": agent.name,
        "symbol": agent.symbol,
        "agentId": if premium { json!({ "name": format!("{}.ai", agent.agent_id_name.clone().unwrap_or_default()), "verifiedAt": agent.agent_id_verified_at.map(iso) }) } else { Value::Null },
        "premium": premium,
        "chain": "eip155:56",
        "wallet": agent.evm_address,
        "owner": agent.owner_account_id,
        "token": agent.token_address.as_ref().map(|t| json!({ "address": t, "launchpad": agent.launchpad })),
        "erc8004": agent.erc8004_agent_id.as_ref().map(|id| json!({ "agentId": id, "chain": e.erc8004_chain, "registry": e.erc8004_registry })),
        "status": public_status(agent),
        "environment": crate::environment::public_environment(agent),
        "issuedAt": iso(chrono::Utc::now()),
    });
    let mut document = document;
    // The agent's OrvynID NFT (non-transferable), once it has one.
    if let Some(nft) = crate::orvyn_id::document_entry(&agent.id).await {
        document["nft"] = nft;
    }
    let Some(key) = e.orvyn_attestation_key.as_deref() else { return Ok(json!({ "document": document, "attestation": null })) };
    let signer: PrivateKeySigner = key.parse().map_err(|_| Error::unavailable("ORVYN_ATTESTATION_KEY isn't a key"))?;
    let signature = signer.sign_message_sync(document.to_string().as_bytes()).map_err(|e| Error::internal(e.to_string()))?;
    Ok(json!({ "document": document, "attestation": { "signer": signer.address().to_string(), "signature": format!("0x{}", hex::encode(signature.as_bytes())) } }))
}
