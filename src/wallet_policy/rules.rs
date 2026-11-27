//! What an agent's wallet may sign, on BNB Chain only. Checked before every signature goes to Privy
//! (guard.rs).
//!
//! Transactions:
//! - BNB and token transfers: only where the owner signed a withdrawal to (grants.rs), exactly.
//! - The Orvyn registry: `pay` (hosting), for this agent's own id; counted against the daily caps.
//! - four.meme TokenManager2 `createToken` (the agent launching its token), launch fee capped; or
//!   Flap's Portal `newTokenV6`, with the agent as beneficiary and its first buy capped.
//! - The ERC-8004 identity registry: `register` only.
//! - Approvals: Permit2 for x402, the registry for hosting in USDT/USDC, trading routers (exact).
//! - Trading, Aster deposits, OrvynPay, Venus supply and Orvyn RNG: their own modules.
//!
//! Messages (EIP-191): OrvynID proofs and four.meme's sign-in. Typed data (EIP-712): x402 Permit2
//! payments and Aster account actions.

use alloy::primitives::U256;
use alloy::sol;
use alloy::sol_types::{SolCall, SolInterface};
use serde_json::Value;

use super::config::cfg;
use super::grants::{verify_grant, OwnerGrant, Withdrawal};
use super::{add_spend, addr, aster, jobs, orvynpay, refuse, rng, trade, usd_micros, venus, x402, Checked, EvmAgent, Spends, TxRequest, TypedData};
use crate::error::Result;

const NATIVE_TOKEN: &str = "0x0000000000000000000000000000000000000000";

sol! {
    interface Registry {
        function pay(string agentId, address token, uint256 amount) payable;
    }
    interface FourMemeLaunch {
        function createToken(bytes args, bytes signature) payable;
    }
    interface Erc8004 {
        function register(string agentURI) returns (uint256);
    }
    interface Erc20 {
        function transfer(address to, uint256 amount) returns (bool);
        function approve(address spender, uint256 amount) returns (bool);
        function transferFrom(address from, address to, uint256 amount) returns (bool);
        function balanceOf(address owner) view returns (uint256);
        function allowance(address owner, address spender) view returns (uint256);
        function totalSupply() view returns (uint256);
        function decimals() view returns (uint8);
        function name() view returns (string);
        function symbol() view returns (string);
    }
}

/// Flap's newTokenV6 selector: newTokenV6((string,string,string,uint8,bytes32,uint8,address,uint256,address,bytes,bytes32,bytes,uint8,uint8,uint16,uint16,uint64,uint64,uint16,uint16,uint16,uint16,uint256,address,address,uint8)).
pub fn flap_new_token_v6_selector() -> [u8; 4] {
    let sig = "newTokenV6((string,string,string,uint8,bytes32,uint8,address,uint256,address,bytes,bytes32,bytes,uint8,uint8,uint16,uint16,uint64,uint64,uint16,uint16,uint16,uint16,uint256,address,address,uint8))";
    alloy::primitives::keccak256(sig.as_bytes())[..4].try_into().unwrap()
}

/// The fields of a Flap launch the policy looks at. The params struct has 26 fields (more than
/// alloy's tuples take), so it's read from the ABI layout: one 32-byte head slot per field.
struct FlapLaunchParams {
    symbol: String,
    quote_token: String,
    quote_amt: U256,
    beneficiary: String,
    permit_data: Vec<u8>,
    extension_data: Vec<u8>,
    commission_receiver: String,
}

fn decode_flap_launch(data: &[u8]) -> Option<FlapLaunchParams> {
    let args = data.get(4..)?;
    let word = |at: usize| -> Option<U256> { args.get(at..at + 32).map(U256::from_be_slice) };
    let base: usize = word(0)?.try_into().ok()?;
    let slot = |i: usize| word(base + 32 * i);
    let address = |i: usize| -> Option<String> {
        let w = slot(i)?;
        (w < (U256::from(1u8) << 160)).then(|| format!("0x{}", hex::encode(&w.to_be_bytes::<32>()[12..])))
    };
    let dynamic = |i: usize| -> Option<Vec<u8>> {
        let off: usize = slot(i)?.try_into().ok()?;
        let at = base.checked_add(off)?;
        let len: usize = word(at)?.try_into().ok()?;
        args.get(at + 32..at.checked_add(32)?.checked_add(len)?).map(<[u8]>::to_vec)
    };
    if base.checked_add(32 * 26)? > args.len() {
        return None;
    }
    Some(FlapLaunchParams {
        symbol: String::from_utf8(dynamic(1)?).ok()?,
        quote_token: address(6)?,
        quote_amt: slot(7)?,
        beneficiary: address(8)?,
        permit_data: dynamic(9)?,
        extension_data: dynamic(11)?,
        commission_receiver: address(24)?,
    })
}

async fn granted_withdrawal(grant: Option<&OwnerGrant>, agent: &EvmAgent) -> Result<Option<Withdrawal>> {
    let Some(grant) = grant else { return Ok(None) };
    verify_grant(grant, &agent.agent_id, &agent.owner).await.map(Some).map_err(|e| {
        if e.kind == crate::error::Kind::Forbidden { e } else { refuse(format!("couldn't verify the owner's grant: {}", e.message)) }
    })
}

fn check_registry_call(data: &[u8], value: U256, agent: &EvmAgent, spends: &mut Spends) -> Result<String> {
    let call = Registry::payCall::abi_decode(data).map_err(|_| refuse("only pay can be called on the registry"))?;
    if call.agentId != agent.agent_id {
        return Err(refuse("an agent may only pay for itself"));
    }
    let token = addr(&call.token);
    if token == NATIVE_TOKEN {
        if value == U256::ZERO {
            return Err(refuse("a BNB payment must attach the BNB"));
        }
        add_spend(spends, "bnb", value);
        return Ok(format!("hosting paid with {value} wei"));
    }
    if !cfg().stablecoins.contains(&token) {
        return Err(refuse("hosting is paid in BNB, USDT or USDC"));
    }
    if value != U256::ZERO {
        return Err(refuse("no BNB may be attached to a token payment"));
    }
    add_spend(spends, "usd", usd_micros(call.amount));
    Ok(format!("hosting paid with {} of {}", call.amount, call.token))
}

pub async fn check_evm_transaction(tx: &TxRequest, agent: &EvmAgent, grant: Option<&OwnerGrant>) -> Result<Checked> {
    let c = cfg();
    if tx.chain_id != c.chain_id {
        return Err(refuse(format!("only BNB Chain (56) is supported, not {}", tx.chain_id)));
    }
    let Some(to_addr) = tx.to else { return Err(refuse("contract creation isn't allowed")) };
    if tx.has_authorization_list {
        return Err(refuse("EIP-7702 authorizations aren't allowed"));
    }
    let to = addr(&to_addr);
    let value = tx.value;
    let data: &[u8] = &tx.data;
    let mut spends = Spends::new();
    let ok = |summary: String, spends: Spends| Ok(Checked::new(summary, spends));

    // BNB transfer.
    if data.is_empty() {
        if let Some(w) = granted_withdrawal(grant, agent).await? {
            if w.asset == "bsc:native" && w.to == to && w.amount == value {
                return Ok(Checked { summary: format!("withdrawal of {value} wei to {to}, signed by owner"), spends, grant_nonce: Some(w.nonce) });
            }
        }
        return Err(refuse("BNB transfers may only go where the owner signed a withdrawal to"));
    }
    if c.registry.as_deref() == Some(to.as_str()) {
        let summary = check_registry_call(data, value, agent, &mut spends)?;
        return ok(summary, spends);
    }
    if let Some(s) = check_flap_launch(&to, data, value, agent, &mut spends)? {
        return ok(s, spends);
    }
    if let Some(s) = trade::check_trade_call(&to, data, value, &agent.address, &mut spends)? {
        return ok(s, spends);
    }
    if let Some(s) = aster::check_aster_transaction(&to, data, value, agent, &mut spends)? {
        return ok(s, spends);
    }
    if let Some(s) = orvynpay::check_orvynpay_transaction(&to, data, value, agent, &mut spends)? {
        return ok(s, spends);
    }
    if let Some(s) = jobs::check_jobs_transaction(&to, data, value, agent, &mut spends)? {
        return ok(s, spends);
    }
    if let Some(s) = venus::check_venus_transaction(&to, data, value, &mut spends)? {
        return ok(s, spends);
    }
    if let Some(s) = rng::check_rng_transaction(&to, data, value, agent, &mut spends)? {
        return ok(s, spends);
    }
    if to == c.fourmeme_manager {
        if FourMemeLaunch::createTokenCall::abi_decode(data).is_err() {
            return Err(refuse("only createToken, buyTokenAMAP and sellToken can be called on four.meme's TokenManager"));
        }
        if value > c.max_launch_fee {
            return Err(refuse(format!("the launch fee may be at most {} wei", c.max_launch_fee)));
        }
        add_spend(&mut spends, "bnb", value);
        return ok(format!("four.meme token launch ({value} wei fee)"), spends);
    }
    if value != U256::ZERO {
        return Err(refuse("BNB may only be attached to registry payments, launches, trades and owner-signed withdrawals"));
    }
    if to == c.erc8004_registry && c.erc8004_chain == "bsc" {
        if Erc8004::registerCall::abi_decode(data).is_err() {
            return Err(refuse("only registering with the ERC-8004 registry is allowed"));
        }
        return ok("ERC-8004 registration".into(), spends);
    }
    let call = Erc20::Erc20Calls::abi_decode(data).map_err(|_| refuse(format!("calls to {to} aren't allowed")))?;
    match call {
        Erc20::Erc20Calls::approve(a) => {
            let spender = addr(&a.spender);
            if let Some(s) = x402::check_permit2_approval(tx.chain_id, &to, &spender, a.amount)? {
                return ok(s, spends);
            }
            if c.registry.as_deref() == Some(spender.as_str()) {
                if !c.stablecoins.contains(&to) {
                    return Err(refuse("the registry may only be approved for USDT or USDC"));
                }
                if usd_micros(a.amount) > c.daily_cap_usd {
                    return Err(refuse("approval above the daily USD cap"));
                }
                return ok(format!("approve {} of {to} for the registry", a.amount), spends);
            }
            if let Some(stable) = jobs::is_jobs_approval(&to, &spender) {
                if !stable {
                    return Err(refuse("the jobs escrow may only be approved for USDT or USDC"));
                }
                if usd_micros(a.amount) > c.daily_cap_pay_usd {
                    return Err(refuse("approval above the daily payment cap"));
                }
                return ok(format!("approve {} of {to} for the jobs escrow", a.amount), spends);
            }
            if let Some(s) = trade::check_trade_approval(&to, &a.spender.to_string(), a.amount)? {
                return ok(s, spends);
            }
            Err(refuse("approvals may only go to Permit2 (x402), the Orvyn registry, the jobs escrow or a trading router"))
        }
        Erc20::Erc20Calls::transfer(t) => {
            let recipient = addr(&t.to);
            if let Some(w) = granted_withdrawal(grant, agent).await? {
                if w.asset == format!("bsc:{to}") && w.to == recipient && w.amount == t.amount {
                    return Ok(Checked { summary: format!("withdrawal of {} {to} to {recipient}, signed by owner", t.amount), spends, grant_nonce: Some(w.nonce) });
                }
            }
            Err(refuse("token transfers may only go where the owner signed a withdrawal to"))
        }
        other => Err(refuse(format!("{} isn't allowed", erc20_name(&other)))),
    }
}

fn erc20_name(call: &Erc20::Erc20Calls) -> &'static str {
    match call {
        Erc20::Erc20Calls::transfer(_) => "transfer",
        Erc20::Erc20Calls::approve(_) => "approve",
        Erc20::Erc20Calls::transferFrom(_) => "transferFrom",
        Erc20::Erc20Calls::balanceOf(_) => "balanceOf",
        Erc20::Erc20Calls::allowance(_) => "allowance",
        Erc20::Erc20Calls::totalSupply(_) => "totalSupply",
        Erc20::Erc20Calls::decimals(_) => "decimals",
        Erc20::Erc20Calls::name(_) => "name",
        Erc20::Erc20Calls::symbol(_) => "symbol",
    }
}

/// EIP-191 messages: OrvynID proofs, and four.meme's sign-in (to launch the agent's token).
pub fn check_evm_message(raw: &[u8]) -> Result<String> {
    let only = || refuse("only OrvynID proofs and four.meme sign-ins can be signed");
    let text = std::str::from_utf8(raw).map_err(|_| only())?;
    if let Some(id) = text.strip_prefix("You are sign in Meme ") {
        if (1..=128).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Ok("four.meme sign-in".into());
        }
    }
    let claim: Value = serde_json::from_str(text).map_err(|_| only())?;
    if claim.get("type").and_then(Value::as_str) != Some("orvyn-id-proof") {
        return Err(only());
    }
    Ok("OrvynID proof".into())
}

/// EIP-712: x402 Permit2 payments, and Aster account actions.
pub fn check_typed_data(typed: &TypedData, agent: &EvmAgent) -> Result<Checked> {
    if x402::is_permit2_domain(typed) {
        return x402::check_x402_permit(typed, agent);
    }
    if aster::is_aster_domain(typed) {
        return aster::check_aster_main_typed_data(typed, agent);
    }
    Err(refuse("only x402 Permit2 payments and Aster account actions can be signed as typed data"))
}

/// Flap `newTokenV6`: the agent launching its own tax token, with itself as the beneficiary.
fn check_flap_launch(to: &str, data: &[u8], value: U256, agent: &EvmAgent, spends: &mut Spends) -> Result<Option<String>> {
    let c = cfg();
    if c.flap_portal.as_deref() != Some(to) || !data.starts_with(&flap_new_token_v6_selector()) {
        return Ok(None);
    }
    let p = decode_flap_launch(data).ok_or_else(|| refuse("malformed Flap launch"))?;
    if p.beneficiary != agent.address.to_lowercase() {
        return Err(refuse("the token's beneficiary must be the agent itself"));
    }
    if p.quote_token != NATIVE_TOKEN || p.quote_amt != value {
        return Err(refuse("a Flap launch is quoted in BNB, with the first buy attached"));
    }
    if p.commission_receiver != NATIVE_TOKEN || !p.extension_data.is_empty() || !p.permit_data.is_empty() {
        return Err(refuse("no commissions, extensions or permits on a launch"));
    }
    if value > c.max_launch_fee {
        return Err(refuse(format!("the first buy may be at most {} wei", c.max_launch_fee)));
    }
    add_spend(spends, "bnb", value);
    Ok(Some(format!("Flap token launch ({}, first buy {value} wei)", p.symbol)))
}
