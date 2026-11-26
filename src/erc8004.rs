//! ERC-8004 ("Trustless Agents") mirror of an agent's OrvynID. The agent registers itself in the
//! Identity Registry from its own wallet, so it owns the entry. The entry's agentURI is the
//! registration file Orvyn serves at https://<handle>.orvyn.cash/.well-known/agent-registration.json.

use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use serde_json::{json, Value};

use crate::db::{db, Agent};
use crate::env::env;
use crate::environment::public_environment;
use crate::error::Result;
use crate::evm::{self, rpc_error};

sol! {
    interface IdentityRegistry {
        function register(string agentURI) returns (uint256 agentId);
        function setAgentURI(uint256 agentId, string newURI);
        function tokenURI(uint256 tokenId) view returns (string);
        function ownerOf(uint256 tokenId) view returns (address);
        function getAgentWallet(uint256 agentId) view returns (address);
        event Registered(uint256 indexed agentId, string agentURI, address indexed owner);
    }
}

/// 0.0005 of the native coin, at most, per sponsored top-up.
const MAX_SPONSOR_WEI: u64 = 500_000_000_000_000;

pub fn erc8004_chain_id() -> u64 {
    evm::evm_chain(&env().erc8004_chain).map(|c| c.chain_id).unwrap_or(56)
}

pub fn root_domain() -> String {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    var("NEXT_PUBLIC_ROOT_DOMAIN").or_else(|| var("ORVYN_ROOT_DOMAIN")).unwrap_or_else(|| "orvyn.cash".into()).to_lowercase()
}

pub fn agent_uri(handle: &str) -> String {
    format!("https://{handle}.{}/.well-known/agent-registration.json", root_domain())
}

/// The ERC-8004 registration file (registration-v1) for an agent.
pub fn registration_document(agent: &Agent) -> Value {
    let page = format!("https://{}.{}", agent.handle.as_deref().unwrap_or(""), root_domain());
    let api = std::env::var("NEXT_PUBLIC_API_URL").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| format!("https://api.{}", root_domain())).trim_end_matches('/').to_string();
    let description = if agent.description.is_empty() {
        format!("{} is an AI agent on Orvyn{}.", agent.name, if agent.token_address.is_some() { format!(" with its own token ${}", agent.symbol) } else { String::new() })
    } else {
        agent.description.clone()
    };
    let mut services = vec![json!({ "name": "web", "endpoint": page }), json!({ "name": "OrvynID", "endpoint": format!("{page}/.well-known/orvyn-id.json"), "version": "v1" })];
    if let Some(n) = &agent.agent_id_name {
        services.push(json!({ "name": "BinanceAgentID", "endpoint": format!("{n}.ai") }));
    }
    services.push(json!({ "name": "API", "endpoint": format!("{page}/api") }));
    services.push(json!({ "name": "MCP", "endpoint": format!("{api}/mcp") }));
    let registrations = match &agent.erc8004_agent_id {
        Some(id) => json!([{ "agentId": id.parse::<u64>().unwrap_or(0), "agentRegistry": format!("eip155:{}:{}", erc8004_chain_id(), env().erc8004_registry) }]),
        None => json!([]),
    };
    json!({
        "type": "https://eips.ethereum.org/EIPS/eip-8004#registration-v1",
        "name": format!("{} (${})", agent.name, agent.symbol),
        "description": description,
        "image": agent.icon.clone().unwrap_or_else(|| format!("{page}/icon.svg")),
        "services": services,
        "x402Support": true,
        "active": agent.status == "live" && agent.subscription_active,
        "registrations": registrations,
        "supportedTrust": ["reputation"],
        "orvynEnvironment": public_environment(agent),
    })
}

async fn fail(agent: &Agent, message: &str) -> Result<Agent> {
    Ok(sqlx::query_as("update agents set erc8004_error = $2 where id = $1 returning *").bind(&agent.id).bind(crate::http::clip(message, 500)).fetch_one(db()).await?)
}

/// Register the agent in the ERC-8004 Identity Registry (idempotent). Failures are recorded on the
/// agent and retried on a later pass.
pub async fn mirror_to_erc8004(agent: Agent) -> Result<Agent> {
    let e = env();
    let Some(handle) = agent.handle.clone() else { return Ok(agent) };
    if !e.erc8004_enabled || agent.erc8004_agent_id.is_some() {
        return Ok(agent);
    }
    let result = async {
        let chain = evm::evm_chain(&e.erc8004_chain)?;
        let registry = evm::parse_address(&e.erc8004_registry)?;
        let p = evm::provider(&e.erc8004_chain)?;
        let owner = evm::parse_address(&agent.evm_address)?;
        let data = IdentityRegistry::registerCall { agentURI: agent_uri(&handle) }.abi_encode();
        let gas = p.estimate_gas(TransactionRequest::default().from(owner).to(registry).input(data.clone().into())).await.map_err(rpc_error)?;
        let per_gas = p.get_gas_price().await.map_err(rpc_error)?;
        let needed = U256::from(gas) * U256::from(per_gas) * U256::from(3u8) / U256::from(2u8);
        let balance = p.get_balance(owner).await.map_err(rpc_error)?;
        if balance < needed {
            let top_up = needed * U256::from(2u8) - balance;
            let amount = |w: U256| crate::auth::js_number(evm::to_bnb(w));
            let Some(sponsor) = &e.orvyn_erc8004_sponsor_key else {
                return Ok(Err(format!("needs about {} {} on {} for gas", amount(needed), chain.native_symbol, chain.label)));
            };
            if top_up > U256::from(MAX_SPONSOR_WEI) {
                return Ok(Err(format!("gas on {} is unusually high; not sponsoring {} {}", chain.label, amount(top_up), chain.native_symbol)));
            }
            crate::wallets::send_from_key(sponsor, owner, Default::default(), top_up).await?;
        }
        let tx = crate::wallets::agent_send(&agent.id, &agent.evm_wallet, registry, data.into(), U256::ZERO).await?;
        let receipt = crate::wallets::receipt_of(tx).await?.ok_or_else(|| crate::Error::internal("no receipt"))?;
        let id = receipt
            .inner
            .logs()
            .iter()
            .filter(|l| l.address() == registry)
            .find_map(|l| IdentityRegistry::Registered::decode_log(&l.inner).ok().map(|ev| ev.agentId));
        Ok::<_, crate::Error>(id.map(|i| i.to_string()).ok_or_else(|| format!("no Registered event in {tx:#x}")))
    }
    .await;
    match result {
        Ok(Ok(id)) => Ok(sqlx::query_as("update agents set erc8004_agent_id = $2, erc8004_error = null where id = $1 returning *").bind(&agent.id).bind(id).fetch_one(db()).await?),
        Ok(Err(message)) => fail(&agent, &message).await,
        Err(e) => fail(&agent, &e.message).await,
    }
}
