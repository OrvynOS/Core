//! Venus lending (BNB Chain core pool), supply side only: an agent may supply USDT, USDC or BNB to
//! earn interest and take it back. No borrowing, no entering markets as collateral, no vToken
//! transfers. Venus pays redemptions to the caller, so withdrawn funds can only return to the agent.
//! Supplies count against daily caps (lend-usd, lend-bnb); approvals must be exact and go only to
//! the matching market.

use alloy::primitives::U256;
use alloy::sol;
use alloy::sol_types::{SolCall, SolInterface};

use super::config::cfg;
use super::{add_spend, addr, refuse, usd_micros, Spends};
use crate::error::Result;

pub const COMPTROLLER: &str = "0xfd36e2c2a6789db23113685031d7f16329158384";

pub struct VenusMarket {
    pub vtoken: &'static str,
    pub symbol: &'static str,
    /// None: vBNB takes native BNB.
    pub underlying: Option<&'static str>,
}

pub const MARKETS: &[VenusMarket] = &[
    VenusMarket { vtoken: "0xfd5840cd36d94d7229439859c0112a4185bc0255", symbol: "USDT", underlying: Some("0x55d398326f99059ff775485246999027b3197955") },
    VenusMarket { vtoken: "0xeca88125a5adbe82614ffc12d0db554e2e2867c8", symbol: "USDC", underlying: Some("0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d") },
    VenusMarket { vtoken: "0xa07c5b74c9b40447a954e1466938b865b6bbea36", symbol: "BNB", underlying: None },
];

sol! {
    interface VToken {
        function mint(uint256 mintAmount) returns (uint256);
        function redeem(uint256 redeemTokens) returns (uint256);
        function redeemUnderlying(uint256 redeemAmount) returns (uint256);
    }
    interface VBnb {
        function mint() payable;
        function redeem(uint256 redeemTokens) returns (uint256);
        function redeemUnderlying(uint256 redeemAmount) returns (uint256);
    }
    interface Erc20ApproveV {
        function approve(address spender, uint256 amount) returns (bool);
    }
}

pub fn venus_contracts() -> Vec<&'static str> {
    MARKETS.iter().map(|m| m.vtoken).collect()
}

const ONLY: &str = "on Venus only supplying (mint) and withdrawing (redeem, redeemUnderlying) are allowed: no borrowing or collateral";

pub fn check_venus_transaction(to: &str, data: &[u8], value: U256, spends: &mut Spends) -> Result<Option<String>> {
    if let Some(market) = MARKETS.iter().find(|m| m.vtoken == to) {
        let (mint, amount): (bool, U256) = if market.underlying.is_none() {
            match VBnb::VBnbCalls::abi_decode(data).map_err(|_| refuse(ONLY))? {
                VBnb::VBnbCalls::mint(_) => (true, U256::ZERO),
                VBnb::VBnbCalls::redeem(c) => (false, c.redeemTokens),
                VBnb::VBnbCalls::redeemUnderlying(c) => (false, c.redeemAmount),
            }
        } else {
            match VToken::VTokenCalls::abi_decode(data).map_err(|_| refuse(ONLY))? {
                VToken::VTokenCalls::mint(c) => (true, c.mintAmount),
                VToken::VTokenCalls::redeem(c) => (false, c.redeemTokens),
                VToken::VTokenCalls::redeemUnderlying(c) => (false, c.redeemAmount),
            }
        };
        if mint {
            if market.underlying.is_none() {
                if value == U256::ZERO {
                    return Err(refuse("supplying BNB to Venus must attach the BNB"));
                }
                add_spend(spends, "lend-bnb", value);
                return Ok(Some(format!("supply {value} wei BNB to Venus")));
            }
            if value != U256::ZERO {
                return Err(refuse("no BNB may be attached to a token supply"));
            }
            add_spend(spends, "lend-usd", usd_micros(amount));
            return Ok(Some(format!("supply {amount} {} to Venus", market.symbol)));
        }
        if value != U256::ZERO {
            return Err(refuse("no BNB may be attached to a Venus withdrawal"));
        }
        return Ok(Some(format!("withdraw {amount} from Venus {} to the agent", market.symbol)));
    }
    // Approving a stablecoin for its own Venus market: exact amount, within the daily cap.
    if data.starts_with(&[0x09, 0x5e, 0xa7, 0xb3]) {
        let Some(market) = MARKETS.iter().find(|m| m.underlying == Some(to)) else { return Ok(None) };
        let Ok(call) = Erc20ApproveV::approveCall::abi_decode(data) else { return Ok(None) };
        if addr(&call.spender) != market.vtoken {
            return Ok(None);
        }
        if call.amount == U256::MAX {
            return Err(refuse("approvals for Venus must be for the exact amount, not unlimited"));
        }
        if usd_micros(call.amount) > cfg().daily_cap_lend_usd {
            return Err(refuse("approval above the daily lending cap"));
        }
        return Ok(Some(format!("approve {} {} for Venus", call.amount, market.symbol)));
    }
    Ok(None)
}
