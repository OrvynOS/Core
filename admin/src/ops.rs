//! Operations on the live system: API clients, holds, wallet checks, Privy policies, migrations.

use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use orvyn_core::db::{db, log_agent, use_database, Agent, AgentLogKind};
use orvyn_core::env::env;
use orvyn_core::service;
use serde_json::json;

fn core_err(e: orvyn_core::Error) -> anyhow::Error {
    anyhow!(e.message)
}

fn connect() -> Result<()> {
    service::init_env()?;
    use_database(orvyn_core::db::pool_for(&env().database_url, 2).map_err(core_err)?);
    Ok(())
}

async fn find_agent(r: &str) -> Result<Agent> {
    let row: Option<Agent> = sqlx::query_as("select * from agents where id = $1 or handle = $1").bind(r).fetch_optional(db()).await?;
    row.ok_or_else(|| anyhow!("no agent {r}"))
}

sol! {
    struct Client { uint64 registeredAt; uint32 payments; uint256 feesPaid; }
    function getClient(string clientId) view returns (bool found, Client client);
    function registerClient(string clientId);
}

/// Issue an Orvyn API key to an outside app (x402 facilitator users, the public API). Registers
/// the client in OrvynRegistry (its fees are recorded there), then writes "<id>:<key>" to
/// ./<id>.api-key (owner-only). Add that line to ORVYN_API_KEYS, send the key securely, delete the file.
pub async fn api_client(id: Option<&str>, execute: bool) -> Result<()> {
    let Some(id) = id.filter(|i| (1..=64).contains(&i.len()) && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')) else {
        bail!("usage: orvyn-admin api-client <client-id> [--execute]   (client id: 1-64 of A-Z a-z 0-9 _ -)");
    };
    service::init_env()?;
    let registry: Address = std::env::var("ORVYN_REGISTRY_CONTRACT").context("ORVYN_REGISTRY_CONTRACT is required")?.parse()?;
    use alloy::network::TransactionBuilder;
    use alloy::providers::Provider;
    let req = alloy::rpc::types::TransactionRequest::default().with_to(registry).with_input(getClientCall { clientId: id.into() }.abi_encode());
    let out = orvyn_core::evm::bsc().call(req).await.map_err(|e| anyhow!("registry: {e}"))?;
    let found = getClientCall::abi_decode_returns(&out).map(|r| r.found).unwrap_or(false);
    let file = std::env::current_dir()?.join(format!("{id}.api-key"));
    println!("{}: API client \"{id}\" on {registry}", if execute { "EXECUTING" } else { "DRY RUN (add --execute)" });
    println!("{}", if found { "✓ already registered".to_string() } else { format!("{} registerClient(\"{id}\") from the operator wallet", if execute { "→" } else { "·" }) });
    println!("{} write a new key to {}", if execute { "→" } else { "·" }, file.display());
    if !execute {
        return Ok(());
    }
    if !found {
        let key = std::env::var("ORVYN_OPERATOR_PRIVATE_KEY").context("ORVYN_OPERATOR_PRIVATE_KEY (the registry operator wallet) is required")?;
        let hash = orvyn_core::wallets::send_from_key(&key, registry, registerClientCall { clientId: id.into() }.abi_encode().into(), Default::default()).await.map_err(core_err)?;
        println!("✓ registered \"{id}\" ({hash})");
    }
    if file.exists() {
        bail!("{} already exists; move it away first", file.display());
    }
    let api_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    write_private(&file, format!("{id}:{api_key}\n").as_bytes())?;
    println!("✓ wrote {}. Add its line to ORVYN_API_KEYS, send the key securely, then delete the file.", file.display());
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Pause an agent on Orvyn's side (its service is suspended and stays suspended: no jobs,
/// autopilot, chat, billing or on-chain actions), or release the pause (the next billing pass
/// resumes it as usual).
pub async fn hold(r: Option<&str>, reason: Option<&str>) -> Result<()> {
    let (Some(r), Some(reason)) = (r, reason) else { bail!("usage: orvyn-admin hold <agent id | handle> \"<reason>\" | --release") };
    connect()?;
    let agent = find_agent(r).await?;
    let name = agent.handle.clone().unwrap_or_else(|| agent.id.clone());
    if reason == "--release" {
        sqlx::query("update agents set held_reason = null where id = $1").bind(&agent.id).execute(db()).await?;
        log_agent(&agent.id, AgentLogKind::Launch, "Pause by Orvyn released; the agent resumes on its next billing check").await.map_err(core_err)?;
        println!("{name}: hold released (resumes within ~5 minutes if its subscription is active)");
        return Ok(());
    }
    let held: Agent = sqlx::query_as("update agents set held_reason = $2 where id = $1 returning *").bind(&agent.id).bind(reason).fetch_one(db()).await?;
    let after = orvyn_core::hosting::suspend_host(held, &format!("paused by Orvyn: {reason}")).await.map_err(core_err)?;
    println!("{name}: held (\"{reason}\"), hosting {}, Render service {}", after.hosting_status, agent.render_service_id.as_deref().unwrap_or("none"));
    Ok(())
}

/// Can an agent's wallet sign? Shows its custody and signs a harmless OrvynID-style proof (which
/// the wallet policy allows), then checks it recovers to the agent's address. Prints no secrets.
pub async fn wallet_check(r: Option<&str>) -> Result<()> {
    let Some(r) = r else { bail!("usage: orvyn-admin wallet-check <agent id | handle> | --new") };
    let (label, id, meta, address) = if r == "--new" {
        service::init_env()?;
        let id = format!("orvyn-wallet-check-{}", chrono::Utc::now().timestamp_millis());
        let w = orvyn_core::wallets::create_agent_wallets(&id).await.map_err(core_err)?;
        ("new throwaway wallet", id, serde_json::to_value(&w.evm_wallet)?, w.evm_address)
    } else {
        connect()?;
        let a = find_agent(r).await?;
        println!("agent {} ({}), status {}", a.id, a.handle.as_deref().unwrap_or("-"), a.status);
        ("EVM wallet", a.id, a.evm_wallet, a.evm_address)
    };
    let custody = match orvyn_core::wallets::WalletMetadata::from_value(&meta) {
        Ok(orvyn_core::wallets::WalletMetadata::Privy { wallet_id, .. }) => format!("Privy wallet {wallet_id}"),
        Ok(orvyn_core::wallets::WalletMetadata::Local { .. }) => "local key".into(),
        Err(_) => "UNKNOWN".into(),
    };
    println!("{label}: address {address}, custody {custody}");
    let message = json!({ "type": "orvyn-id-proof", "purpose": "wallet check", "at": chrono::Utc::now().to_rfc3339() }).to_string();
    let signed = async {
        let wallet = orvyn_core::wallets::agent_wallet(&id, &meta)?;
        wallet.sign_message(message.as_bytes()).await
    }
    .await;
    match signed {
        Ok(sig) => {
            let bytes = hex::decode(sig.trim_start_matches("0x")).unwrap_or_default();
            match orvyn_core::evm::sig::recover(orvyn_core::evm::sig::message_hash(&message), &bytes) {
                Some(a) if format!("{a:#x}").eq_ignore_ascii_case(&address) => println!("signs: OK"),
                Some(a) => println!("signs: MISMATCH (recovered {a})"),
                None => println!("signs: FAILED: the signature doesn't recover"),
            }
        }
        Err(e) => println!("signs: FAILED: {}", e.message),
    }
    Ok(())
}

/// Create Orvyn's Privy wallet policies (agent wallets, and their Aster API wallets) and print
/// their ids for PRIVY_AGENT_POLICY_ID and PRIVY_ASTER_POLICY_ID; --print only shows the rules.
pub async fn privy_policy(print: bool) -> Result<()> {
    use orvyn_core::wallet_policy::privy_policy::{agent_policy_rules, aster_signer_policy_rules, create_policies};
    if std::env::var("ORVYN_REGISTRY_CONTRACT").map_or(true, |v| v.is_empty()) {
        eprintln!("warning: ORVYN_REGISTRY_CONTRACT isn't set, so the policy won't allow paying the registry");
    }
    if print {
        println!("{}", serde_json::to_string_pretty(&json!({ "agent": agent_policy_rules(), "asterSigner": aster_signer_policy_rules() }))?);
        return Ok(());
    }
    service::init_env()?;
    if env().privy_authorization_key_id.is_none() {
        eprintln!("warning: PRIVY_AUTHORIZATION_KEY_ID isn't set, so the policy has no owner and the app secret alone can change it");
    }
    let (agent, aster) = create_policies().await.map_err(core_err)?;
    let count = |p: &serde_json::Value| p["rules"].as_array().map_or(0, Vec::len);
    let (a, s) = (agent["id"].as_str().unwrap_or("?"), aster["id"].as_str().unwrap_or("?"));
    println!("created policy {a} ({} rules) and {s} ({})", count(&agent), count(&aster));
    println!("set PRIVY_AGENT_POLICY_ID={a} and PRIVY_ASTER_POLICY_ID={s} on the API and orchestrator");
    Ok(())
}

/// crates/core/migrations, found from the working directory (a checkout of the monorepo or of
/// orvyn-core).
fn migrations_dir() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        for candidate in [dir.join("crates/core/migrations"), dir.join("migrations")] {
            if candidate.join("meta/_journal.json").exists() {
                return Ok(candidate);
            }
        }
        if !dir.pop() {
            bail!("no migrations folder (crates/core/migrations) above the working directory");
        }
    }
}

/// Add an empty migration and its journal entry (the build embeds what the journal lists). Write
/// the SQL; separate statements with `--> statement-breakpoint`. Never edit an applied migration.
pub fn new_migration(name: Option<&str>) -> Result<()> {
    let Some(name) = name.filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')) else {
        bail!("usage: orvyn-admin new-migration <name>   (lowercase, digits and _)");
    };
    let dir = migrations_dir()?;
    let journal_path = dir.join("meta/_journal.json");
    let mut journal: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&journal_path)?)?;
    let entries = journal["entries"].as_array_mut().context("journal has no entries")?;
    let idx = entries.len();
    let last_when = entries.last().and_then(|e| e["when"].as_i64()).unwrap_or(0);
    let tag = format!("{idx:04}_{name}");
    let file = dir.join(format!("{tag}.sql"));
    if file.exists() {
        bail!("{} already exists", file.display());
    }
    // Strictly increasing: drizzle's bookkeeping applies migrations newer than the last one recorded.
    let when = chrono::Utc::now().timestamp_millis().max(last_when + 1);
    entries.push(json!({ "idx": idx, "version": "7", "when": when, "tag": tag, "breakpoints": true }));
    std::fs::write(&file, format!("-- {tag}: describe the change. Separate statements with the breakpoint marker.\n"))?;
    // The journal in drizzle's own layout and key order, so it diffs as one added entry.
    let entry = |e: &serde_json::Value| {
        format!(
            "    {{\n      \"idx\": {},\n      \"version\": {},\n      \"when\": {},\n      \"tag\": {},\n      \"breakpoints\": {}\n    }}",
            e["idx"], e["version"], e["when"], e["tag"], e["breakpoints"]
        )
    };
    let body = journal["entries"].as_array().unwrap().iter().map(entry).collect::<Vec<_>>().join(",\n");
    let text = format!("{{\n  \"version\": {},\n  \"dialect\": {},\n  \"entries\": [\n{body}\n  ]\n}}", journal["version"], journal["dialect"]);
    std::fs::write(&journal_path, text)?;
    println!("wrote {} and its journal entry; fill in the SQL, then `cargo test -p orvyn-core --test foundation --test schema`", file.display());
    Ok(())
}
