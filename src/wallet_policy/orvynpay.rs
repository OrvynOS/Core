//! OrvynPay calls an agent's wallet may sign:
//! - OrvynPay `pay` in BNB, USDT or USDC to anyone else, and exact approvals of OrvynPay for
//!   USDT/USDC. Counted against the daily payment caps.
//! - The shielded pool: `registerKey`; `transact` deposits (counted against the same caps) and
//!   withdrawals only to the agent itself, with no relayer fee; exact approvals of the pool.

use alloy::primitives::{I256, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolInterface};

use super::config::cfg;
use super::{add_spend, addr, refuse, usd_micros, EvmAgent, Spends};
use crate::error::Result;

const NATIVE: &str = "0x0000000000000000000000000000000000000000";

sol! {
    interface OrvynPayAbi {
        function pay(address to, address token, uint256 amount, bytes32 ref) payable;
    }
    struct ProofArgs { uint256[2] a; uint256[2][2] b; uint256[2] c; uint256 root; uint256 keyRoot; uint256 publicAmount; uint256 extDataHash; uint256 asset; uint256[2] inputNullifiers; uint256[2] outputCommitments; }
    struct ExtData { address recipient; int256 extAmount; address relayer; uint256 fee; bytes encryptedOutput1; bytes encryptedOutput2; }
    interface ShieldedPool {
        function registerKey(uint256 shieldedKey, bytes encryptionKey);
        function transact(ProofArgs p, ExtData e) payable;
    }
    interface Erc20ApproveP {
        function approve(address spender, uint256 amount) returns (bool);
    }
}

fn count_payment(spends: &mut Spends, token: &str, amount: U256) -> Result<()> {
    if token == NATIVE {
        add_spend(spends, "pay-bnb", amount);
    } else if cfg().stablecoins.contains(&token.to_string()) {
        add_spend(spends, "pay-usd", usd_micros(amount));
    } else {
        return Err(refuse("OrvynPay payments are in BNB, USDT or USDC"));
    }
    Ok(())
}

/// None if `to` isn't an OrvynPay contract (or an approval of one).
pub fn check_orvynpay_transaction(to: &str, data: &[u8], value: U256, agent: &EvmAgent, spends: &mut Spends) -> Result<Option<String>> {
    let c = cfg();
    if c.orvyn_pay.as_deref() == Some(to) {
        let call = OrvynPayAbi::payCall::abi_decode(data).map_err(|_| refuse("only pay can be called on OrvynPay"))?;
        let (payee, token) = (addr(&call.to), addr(&call.token));
        let (payee_cs, token_cs) = (call.to.to_string(), call.token.to_string());
        if payee == agent.address.to_lowercase() {
            return Err(refuse("an agent can't pay itself"));
        }
        let ok = if token == NATIVE { value == call.amount } else { value == U256::ZERO };
        if !ok {
            return Err(refuse("the BNB attached must match the payment"));
        }
        count_payment(spends, &token, call.amount)?;
        return Ok(Some(format!("OrvynPay {} of {token_cs} to {payee_cs}", call.amount)));
    }
    if c.shielded_pool.as_deref() == Some(to) {
        let call = ShieldedPool::ShieldedPoolCalls::abi_decode(data).map_err(|_| refuse("only registerKey and transact can be called on the shielded pool"))?;
        let t = match call {
            ShieldedPool::ShieldedPoolCalls::registerKey(_) => {
                if value != U256::ZERO {
                    return Err(refuse("no BNB with registerKey"));
                }
                return Ok(Some("register the agent's shielded key".into()));
            }
            ShieldedPool::ShieldedPoolCalls::transact(t) => t,
        };
        let token = format!("0x{:0>40}", format!("{:x}", t.p.asset));
        let ext = t.e;
        if ext.fee != U256::ZERO || addr(&ext.relayer) != NATIVE {
            return Err(refuse("no relayer fees"));
        }
        if ext.extAmount > I256::ZERO {
            let amount = ext.extAmount.into_raw();
            let ok = if token == NATIVE { value == amount } else { value == U256::ZERO };
            if !ok {
                return Err(refuse("the BNB attached must match the deposit"));
            }
            count_payment(spends, &token, amount)?;
            return Ok(Some(format!("shielded deposit of {} of {token}", ext.extAmount)));
        }
        if value != U256::ZERO {
            return Err(refuse("no BNB with a shielded transfer or withdrawal"));
        }
        if ext.extAmount < I256::ZERO && addr(&ext.recipient) != agent.address.to_lowercase() {
            return Err(refuse("shielded withdrawals only go to the agent's own wallet"));
        }
        return Ok(Some(if ext.extAmount < I256::ZERO { format!("shielded withdrawal of {} of {token} to the agent", -ext.extAmount) } else { "shielded transfer".into() }));
    }
    // Exact approvals of OrvynPay or the pool, for stablecoins, within the daily payment cap.
    if c.stablecoins.contains(&to.to_string()) && data.starts_with(&[0x09, 0x5e, 0xa7, 0xb3]) {
        let Ok(call) = Erc20ApproveP::approveCall::abi_decode(data) else { return Ok(None) };
        let s = addr(&call.spender);
        if Some(&s) != c.orvyn_pay.as_ref() && Some(&s) != c.shielded_pool.as_ref() {
            return Ok(None);
        }
        if usd_micros(call.amount) > c.daily_cap_pay_usd {
            return Err(refuse("approval above the daily payment cap"));
        }
        return Ok(Some(format!("approve {} of {to} for OrvynPay", call.amount)));
    }
    Ok(None)
}
