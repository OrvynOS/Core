//! The jobs escrow (OrvynJobs) as a client: an agent hiring another agent may
//! - `fund` a job for someone else (never itself), in BNB, USDT or USDC, the BNB attached matching
//!   the amount; counted against the daily payment caps like OrvynPay;
//! - `approve`, `dispute` or `reclaim` its jobs (moving nothing new);
//! - approve the escrow for exactly the amount, in USDT or USDC.

use alloy::primitives::U256;
use alloy::sol;
use alloy::sol_types::SolCall;

use super::config::cfg;
use super::{add_spend, addr, refuse, usd_micros, EvmAgent, Spends};
use crate::error::Result;

const NATIVE: &str = "0x0000000000000000000000000000000000000000";

sol! {
    interface OrvynJobsClient {
        function fund(string jobId, string agentId, address payee, address token, uint256 amount, uint64 deadline) payable;
        function approve(string jobId);
        function dispute(string jobId, string reason);
        function reclaim(string jobId);
    }
}

/// None if `to` isn't the jobs escrow.
pub fn check_jobs_transaction(to: &str, data: &[u8], value: U256, agent: &EvmAgent, spends: &mut Spends) -> Result<Option<String>> {
    let c = cfg();
    if c.orvyn_jobs.as_deref() != Some(to) {
        return Ok(None);
    }
    if let Ok(call) = OrvynJobsClient::fundCall::abi_decode(data) {
        let (payee, token) = (addr(&call.payee), addr(&call.token));
        if payee == agent.address.to_lowercase() {
            return Err(refuse("an agent can't hire itself"));
        }
        let attached_ok = if token == NATIVE { value == call.amount } else { value == U256::ZERO };
        if !attached_ok {
            return Err(refuse("the BNB attached must match the job's amount"));
        }
        if token == NATIVE {
            add_spend(spends, "pay-bnb", call.amount);
        } else if c.stablecoins.contains(&token) {
            add_spend(spends, "pay-usd", usd_micros(call.amount));
        } else {
            return Err(refuse("jobs are paid in BNB, USDT or USDC"));
        }
        return Ok(Some(format!("hire {} for job {} ({} of {})", call.payee, call.jobId, call.amount, call.token)));
    }
    if value != U256::ZERO {
        return Err(refuse("only funding a job may attach BNB"));
    }
    if let Ok(call) = OrvynJobsClient::approveCall::abi_decode(data) {
        return Ok(Some(format!("approve job {}", call.jobId)));
    }
    if let Ok(call) = OrvynJobsClient::disputeCall::abi_decode(data) {
        return Ok(Some(format!("dispute job {}", call.jobId)));
    }
    if let Ok(call) = OrvynJobsClient::reclaimCall::abi_decode(data) {
        return Ok(Some(format!("reclaim job {}", call.jobId)));
    }
    Err(refuse("only fund, approve, dispute and reclaim can be called on the jobs escrow"))
}

/// Whether approving `spender` for `token` is approving the jobs escrow for a stablecoin.
pub fn is_jobs_approval(token: &str, spender: &str) -> Option<bool> {
    let c = cfg();
    (c.orvyn_jobs.as_deref() == Some(spender)).then(|| c.stablecoins.contains(&token.to_string()))
}
