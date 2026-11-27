//! OrvynID on chain: each agent's non-transferable identity NFT (contracts/src/OrvynID.sol, ERC-5192),
//! held by the agent's own wallet. It names the agent's handle (its token URI is the agent's OrvynID
//! document), its creator, its ERC-8004 entry and its Binance Agent ID, which the contract also
//! checks live against the Agent ID registry.
//!
//! The chain is the source of truth: `sync` reads the agent's token and changes only what differs
//! (issue it, then handle, creator, wallet, ERC-8004 link, Agent ID). Orvyn's operator key is the
//! contract's issuer. Off unless ORVYN_ID_CONTRACT is set.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

use crate::db::{log_agent, Agent, AgentLogKind};
use crate::error::{Error, Result};
use crate::evm::{self, rpc_error};

sol! {
    #[sol(rpc)]
    interface IOrvynID {
        struct Identity {
            bytes32 agentKey;
            address creator;
            uint64 issuedAt;
            uint256 erc8004Id;
            string handle;
            string agentIdName;
        }
        function issue(string agentId, address wallet, address creator, string handle) returns (uint256);
        function setHandle(uint256 tokenId, string handle);
        function setCreator(uint256 tokenId, address creator);
        function linkErc8004(uint256 tokenId, uint256 erc8004Id);
        function recordAgentId(uint256 tokenId, string name);
        function clearAgentId(uint256 tokenId);
        function moveWallet(uint256 tokenId, address newWallet);
        function tokenOfAgentId(string agentId) view returns (uint256);
        function identityOf(uint256 tokenId) view returns (Identity identity, address wallet, bool agentIdHeldNow);
    }
}

/// The OrvynID contract, if this deployment has one.
pub fn contract() -> Option<Address> {
    std::env::var("ORVYN_ID_CONTRACT").ok().filter(|v| !v.is_empty()).and_then(|v| v.parse().ok())
}

pub fn enabled() -> bool {
    contract().is_some()
}

fn operator_key() -> Result<String> {
    crate::env::env().orvyn_operator_private_key.clone().ok_or_else(|| Error::unavailable("ORVYN_OPERATOR_PRIVATE_KEY is not set"))
}

async fn send(data: Vec<u8>) -> Result<()> {
    let to = contract().ok_or_else(|| Error::unavailable("ORVYN_ID_CONTRACT is not set"))?;
    crate::wallets::send_from_key(&operator_key()?, to, data.into(), U256::ZERO).await?;
    Ok(())
}

/// The agent's OrvynID token id (0: none yet).
pub async fn token_of(agent_id: &str) -> Result<U256> {
    let Some(c) = contract() else { return Ok(U256::ZERO) };
    IOrvynID::new(c, evm::bsc()).tokenOfAgentId(agent_id.to_string()).call().await.map_err(rpc_error)
}

/// The Agent ID the token should carry: the agent's verified one, or none.
fn wanted_agent_id(agent: &Agent) -> String {
    match (&agent.agent_id_name, &agent.agent_id_verified_at) {
        (Some(n), Some(_)) => n.to_lowercase(),
        _ => String::new(),
    }
}

/// Bring the agent's OrvynID in line with the agent. Returns what changed.
pub async fn sync(agent: &Agent) -> Result<Vec<&'static str>> {
    let Some(c) = contract() else { return Ok(vec![]) };
    let Some(handle) = agent.handle.clone().filter(|h| !h.is_empty()) else { return Ok(vec![]) };
    if !evm::is_address(&agent.evm_address) || !evm::is_address(&agent.owner_account_id) {
        return Ok(vec![]);
    }
    let wallet = evm::parse_address(&agent.evm_address)?;
    let creator = evm::parse_address(&agent.owner_account_id)?;
    let id = IOrvynID::new(c, evm::bsc());
    let mut changed = vec![];

    let mut token = token_of(&agent.id).await?;
    if token.is_zero() {
        send(IOrvynID::issueCall { agentId: agent.id.clone(), wallet, creator, handle: handle.clone() }.abi_encode()).await?;
        changed.push("issued");
        token = token_of(&agent.id).await?;
        if token.is_zero() {
            return Err(Error::internal("OrvynID: issued, but the token isn't readable yet"));
        }
        log_agent(&agent.id, AgentLogKind::Plugin, &format!("OrvynID #{token} issued to the agent's wallet (non-transferable).")).await?;
    }
    let current = id.identityOf(token).call().await.map_err(rpc_error)?;
    let (ident, holder) = (current.identity, current.wallet);

    if ident.handle != handle {
        send(IOrvynID::setHandleCall { tokenId: token, handle: handle.clone() }.abi_encode()).await?;
        changed.push("handle");
    }
    if ident.creator != creator {
        send(IOrvynID::setCreatorCall { tokenId: token, creator }.abi_encode()).await?;
        changed.push("creator");
    }
    if holder != wallet {
        send(IOrvynID::moveWalletCall { tokenId: token, newWallet: wallet }.abi_encode()).await?;
        changed.push("wallet");
    }
    let erc8004: U256 = agent.erc8004_agent_id.as_deref().and_then(|v| v.parse().ok()).unwrap_or(U256::ZERO);
    if ident.erc8004Id != erc8004 {
        send(IOrvynID::linkErc8004Call { tokenId: token, erc8004Id: erc8004 }.abi_encode()).await?;
        changed.push("erc8004");
    }
    let want = wanted_agent_id(agent);
    if ident.agentIdName != want {
        if want.is_empty() {
            send(IOrvynID::clearAgentIdCall { tokenId: token }.abi_encode()).await?;
            changed.push("agent-id-cleared");
        } else {
            send(IOrvynID::recordAgentIdCall { tokenId: token, name: want.clone() }.abi_encode()).await?;
            changed.push("agent-id");
            log_agent(&agent.id, AgentLogKind::Plugin, &format!("{want}.ai recorded on its OrvynID.")).await?;
        }
    }
    Ok(changed)
}

/// What the OrvynID document says about the NFT, if the agent has one.
pub async fn document_entry(agent_id: &str) -> Option<Value> {
    let c = contract()?;
    let token = token_of(agent_id).await.ok().filter(|t| !t.is_zero())?;
    Some(json!({ "standard": "ERC-721 (ERC-5192, non-transferable)", "chainId": evm::BSC.chain_id, "contract": format!("{c:#x}"), "tokenId": token.to_string() }))
}
