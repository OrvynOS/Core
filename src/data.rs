//! Public market data: DexScreener (pairs), GeckoTerminal (pools, candles), GoPlus (token risk).
//! All keyless.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::http::client;

async fn get_json(url: &str, what: &str) -> Result<Value> {
    let res = client().get(url).header("accept", "application/json").timeout(std::time::Duration::from_secs(15)).send().await.map_err(|e| Error::internal(format!("{what}: {e}")))?;
    if !res.status().is_success() {
        return Err(Error::internal(format!("{what} returned {}", res.status().as_u16())));
    }
    res.json().await.map_err(|e| Error::internal(format!("{what}: {e}")))
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// ---- DexScreener ----

pub mod dexscreener {
    use super::*;
    const BASE: &str = "https://api.dexscreener.com";

    pub async fn search(query: &str) -> Result<Vec<Value>> {
        let v = get_json(&format!("{BASE}/latest/dex/search?q={}", enc(query)), "DexScreener").await?;
        Ok(v["pairs"].as_array().cloned().unwrap_or_default())
    }
    /// All pairs for a token on one chain.
    pub async fn token_pairs(chain: &str, address: &str) -> Result<Vec<Value>> {
        let v = get_json(&format!("{BASE}/tokens/v1/{chain}/{address}"), "DexScreener").await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
    pub async fn top_boosted() -> Result<Vec<Value>> {
        Ok(get_json(&format!("{BASE}/token-boosts/top/v1"), "DexScreener").await?.as_array().cloned().unwrap_or_default())
    }
    /// The deepest pair for a token, which is what prices it.
    pub async fn best_pair(chain: &str, address: &str) -> Result<Option<Value>> {
        let mut pairs: Vec<Value> = token_pairs(chain, address).await?.into_iter().filter(|p| p["baseToken"]["address"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(address))).collect();
        pairs.sort_by(|a, b| liq(b).partial_cmp(&liq(a)).unwrap_or(std::cmp::Ordering::Equal));
        Ok(pairs.into_iter().next())
    }
    fn liq(p: &Value) -> f64 {
        p["liquidity"]["usd"].as_f64().unwrap_or(0.0)
    }
    /// Compact pair view for the model.
    pub fn summarize_pair(p: &Value) -> Value {
        let created = p["pairCreatedAt"].as_f64();
        json!({
            "chain": p["chainId"], "dex": p["dexId"], "pair": p["pairAddress"],
            "token": { "address": p["baseToken"]["address"], "symbol": p["baseToken"]["symbol"], "name": p["baseToken"]["name"] },
            "quote": p["quoteToken"]["symbol"],
            "price_usd": p["priceUsd"].as_str().and_then(|s| s.parse::<f64>().ok()),
            "liquidity_usd": liq(p).round(),
            "market_cap_usd": if p["marketCap"].is_null() { p["fdv"].clone() } else { p["marketCap"].clone() },
            "volume_24h_usd": p["volume"]["h24"].as_f64().unwrap_or(0.0).round(),
            "change_pct": { "h1": p["priceChange"]["h1"], "h24": p["priceChange"]["h24"] },
            "txns_24h": p["txns"]["h24"],
            "age_hours": created.map(|c| ((chrono::Utc::now().timestamp_millis() as f64 - c) / 3_600_000.0).round()),
            "url": p["url"],
        })
    }
}

// ---- GeckoTerminal ----

pub mod geckoterminal {
    use super::*;
    const BASE: &str = "https://api.geckoterminal.com/api/v2";

    fn num(v: &Value) -> Option<f64> {
        v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64())
    }

    pub fn summarize_pool(p: &Value) -> Value {
        let a = &p["attributes"];
        let network = p["id"].as_str().unwrap_or("").split('_').next().unwrap_or("").to_string();
        let base = p["relationships"]["base_token"]["data"]["id"].as_str().map(|id| id.split('_').skip(1).collect::<Vec<_>>().join("_"));
        json!({
            "network": network, "pool": a["address"], "name": a["name"],
            "dex": p["relationships"]["dex"]["data"]["id"], "base_token": base,
            "price_usd": num(&a["base_token_price_usd"]),
            "liquidity_usd": num(&a["reserve_in_usd"]).unwrap_or(0.0).round(),
            "fdv_usd": num(&a["fdv_usd"]),
            "volume_24h_usd": num(&a["volume_usd"]["h24"]).unwrap_or(0.0).round(),
            "change_pct": { "h1": num(&a["price_change_percentage"]["h1"]), "h24": num(&a["price_change_percentage"]["h24"]) },
            "txns_24h": a["transactions"]["h24"], "created_at": a["pool_created_at"],
        })
    }

    pub async fn trending_pools(network: Option<&str>) -> Result<Vec<Value>> {
        let path = match network {
            Some(n) => format!("/networks/{n}/trending_pools?page=1"),
            None => "/networks/trending_pools?page=1".into(),
        };
        let v = get_json(&format!("{BASE}{path}"), "GeckoTerminal").await?;
        Ok(v["data"].as_array().map(|a| a.iter().map(summarize_pool).collect()).unwrap_or_default())
    }
    pub async fn search_pools(query: &str, network: Option<&str>) -> Result<Vec<Value>> {
        let n = network.map(|n| format!("&network={n}")).unwrap_or_default();
        let v = get_json(&format!("{BASE}/search/pools?query={}{n}", enc(query)), "GeckoTerminal").await?;
        Ok(v["data"].as_array().map(|a| a.iter().map(summarize_pool).collect()).unwrap_or_default())
    }
    pub async fn pool(network: &str, pool: &str) -> Result<Value> {
        Ok(summarize_pool(&get_json(&format!("{BASE}/networks/{network}/pools/{pool}"), "GeckoTerminal").await?["data"]))
    }

    #[derive(Debug, Clone, Copy, serde::Serialize)]
    pub struct Candle {
        pub t: i64,
        pub o: f64,
        pub h: f64,
        pub l: f64,
        pub c: f64,
        pub v: f64,
    }

    /// Candles oldest first. timeframe: minute|hour|day.
    pub async fn ohlcv(network: &str, pool: &str, timeframe: &str, aggregate: u32, limit: u32) -> Result<Vec<Candle>> {
        let v = get_json(&format!("{BASE}/networks/{network}/pools/{pool}/ohlcv/{timeframe}?aggregate={aggregate}&limit={limit}"), "GeckoTerminal").await?;
        let mut out: Vec<Candle> = v["data"]["attributes"]["ohlcv_list"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let n = |i: usize| r[i].as_f64();
                        Some(Candle { t: (n(0)? * 1000.0) as i64, o: n(1)?, h: n(2)?, l: n(3)?, c: n(4)?, v: n(5)? })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|c| c.t);
        Ok(out)
    }
}

// ---- GoPlus ----

pub mod goplus {
    use super::*;
    const BASE: &str = "https://api.gopluslabs.io/api/v1";

    async fn get(path: &str) -> Result<Value> {
        let body = get_json(&format!("{BASE}{path}"), "GoPlus").await?;
        if body["code"].as_i64() != Some(1) {
            return Err(Error::internal(format!("GoPlus: {}", body["message"].as_str().unwrap_or("error"))));
        }
        Ok(body["result"].clone())
    }

    pub async fn token_security(goplus_chain: &str, address: &str) -> Result<Option<Value>> {
        let r = get(&format!("/token_security/{goplus_chain}?contract_addresses={}", address.to_lowercase())).await?;
        Ok(r.get(address.to_lowercase()).cloned().filter(|v| !v.is_null()))
    }
    pub async fn address_security(address: &str, goplus_chain: Option<&str>) -> Result<Value> {
        get(&format!("/address_security/{address}{}", goplus_chain.map(|c| format!("?chain_id={c}")).unwrap_or_default())).await
    }

    #[derive(Debug, Clone, Copy, Deserialize)]
    pub struct SecurityPolicy {
        pub max_tax_pct: f64,
        pub require_open_source: bool,
        pub allow_proxy: bool,
        pub allow_mintable: bool,
    }
    pub const DEFAULT_POLICY: SecurityPolicy = SecurityPolicy { max_tax_pct: 5.0, require_open_source: true, allow_proxy: false, allow_mintable: false };

    fn on(r: &Value, k: &str) -> bool {
        r[k] == "1"
    }
    fn pct(r: &Value, k: &str) -> f64 {
        r[k].as_str().and_then(|s| s.parse::<f64>().ok()).map(|v| v * 100.0).unwrap_or(0.0)
    }

    #[derive(Debug, Clone, serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Assessment {
        pub ok: bool,
        pub reasons: Vec<String>,
        pub warnings: Vec<String>,
        pub buy_tax_pct: Option<f64>,
        pub sell_tax_pct: Option<f64>,
    }

    /// Pass/fail with human-readable reasons. Unknown tokens fail closed.
    pub fn assess_token(report: Option<&Value>, policy: SecurityPolicy) -> Assessment {
        let Some(r) = report else {
            return Assessment { ok: false, reasons: vec!["GoPlus has no security data for this token.".into()], warnings: vec![], buy_tax_pct: None, sell_tax_pct: None };
        };
        let (buy, sell) = (pct(r, "buy_tax"), pct(r, "sell_tax"));
        let mut reasons = vec![];
        let checks = [
            ("is_honeypot", "honeypot: tokens can't be sold"),
            ("cannot_buy", "buying is disabled"),
            ("cannot_sell_all", "holders can't sell their full balance"),
            ("owner_change_balance", "owner can change balances"),
            ("hidden_owner", "hidden owner"),
            ("can_take_back_ownership", "ownership can be reclaimed"),
            ("selfdestruct", "contract can self-destruct"),
            ("transfer_pausable", "transfers can be paused"),
            ("trading_cooldown", "trading cooldown"),
        ];
        for (k, why) in checks {
            if on(r, k) {
                reasons.push(why.to_string());
            }
        }
        if on(r, "slippage_modifiable") || on(r, "personal_slippage_modifiable") {
            reasons.push("tax can be changed by the owner".into());
        }
        let js = crate::auth::js_number;
        if buy > policy.max_tax_pct || sell > policy.max_tax_pct {
            reasons.push(format!("tax {}%/{}% over {}%", js(buy), js(sell), js(policy.max_tax_pct)));
        }
        if policy.require_open_source && r["is_open_source"] != "1" {
            reasons.push("contract source isn't verified".into());
        }
        if !policy.allow_proxy && on(r, "is_proxy") {
            reasons.push("upgradeable proxy contract".into());
        }
        if !policy.allow_mintable && on(r, "is_mintable") {
            reasons.push("supply can be minted".into());
        }
        let mut warnings = vec![];
        let n = |k: &str| r[k].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        if n("honeypot_with_same_creator") > 0.0 {
            warnings.push(format!("creator address is linked to {} honeypot(s)", r["honeypot_with_same_creator"].as_str().unwrap_or("")));
        }
        if n("owner_percent") > 0.1 {
            warnings.push(format!("owner holds {:.1}% of supply", n("owner_percent") * 100.0));
        }
        Assessment { ok: reasons.is_empty(), reasons, warnings, buy_tax_pct: Some(buy), sell_tax_pct: Some(sell) }
    }

    /// Compact report for the model.
    pub fn summarize_security(report: Option<&Value>) -> Value {
        let Some(r) = report else { return Value::Null };
        let flags: Vec<&String> = r.as_object().map(|o| o.iter().filter(|(k, v)| *v == "1" && *k != "is_open_source" && *k != "is_in_dex").map(|(k, _)| k).collect()).unwrap_or_default();
        let n = |k: &str| r[k].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let mut out = json!({
            "name": r["token_name"], "symbol": r["token_symbol"],
            "buy_tax_pct": pct(r, "buy_tax"), "sell_tax_pct": pct(r, "sell_tax"),
            "open_source": r["is_open_source"] == "1",
            "holders": n("holder_count"), "owner_percent": n("owner_percent") * 100.0, "creator_percent": n("creator_percent") * 100.0,
            "risk_flags": flags,
        });
        // Absent fields are left out (as JSON.stringify drops undefined).
        for k in ["name", "symbol"] {
            if out[k].is_null() {
                out.as_object_mut().unwrap().remove(k);
            }
        }
        out
    }
}
