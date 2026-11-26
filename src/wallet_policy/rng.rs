//! Orvyn RNG (contracts/src/rng/OrvynRNG.sol): an agent may register its node (staking, with itself
//! as the payee, at most WALLET_MAX_RNG_STAKE_BNB), exit, and withdraw its stake and earnings.

use alloy::primitives::U256;
use alloy::sol;
use alloy::sol_types::SolInterface;

use super::config::cfg;
use super::{add_spend, addr, refuse, EvmAgent, Spends};
use crate::error::Result;

sol! {
    interface OrvynRng {
        function register(bytes publicKey, address payee, bytes proof) payable returns (uint256);
        function exit(uint256 index);
        function withdrawStake(uint256 index);
        function withdraw();
    }
}

pub fn check_rng_transaction(to: &str, data: &[u8], value: U256, agent: &EvmAgent, spends: &mut Spends) -> Result<Option<String>> {
    let c = cfg();
    if c.orvyn_rng.as_deref() != Some(to) {
        return Ok(None);
    }
    let call = OrvynRng::OrvynRngCalls::abi_decode(data).map_err(|_| refuse("on Orvyn RNG an agent may only register its node, exit, and withdraw its stake and earnings"))?;
    let name = match call {
        OrvynRng::OrvynRngCalls::register(r) => {
            if addr(&r.payee) != agent.address.to_lowercase() {
                return Err(refuse("an Orvyn RNG node must pay its earnings to the agent itself"));
            }
            if value > c.max_rng_stake {
                return Err(refuse(format!("an Orvyn RNG stake may be at most {} wei", c.max_rng_stake)));
            }
            add_spend(spends, "bnb", value);
            return Ok(Some(format!("stake {value} wei to run an Orvyn RNG node")));
        }
        OrvynRng::OrvynRngCalls::exit(_) => "exit",
        OrvynRng::OrvynRngCalls::withdrawStake(_) => "withdrawStake",
        OrvynRng::OrvynRngCalls::withdraw(_) => "withdraw",
    };
    if value != U256::ZERO {
        return Err(refuse("no BNB may be attached to this Orvyn RNG call"));
    }
    Ok(Some(format!("Orvyn RNG {name}")))
}
