//! Live checks against BNB Chain (read-only): every venue's quote and build tools through the HTTP
//! MCP endpoint, each plan dry-run (eth_simulateV1) from a funded mainnet wallet. Nothing is
//! signed or sent. Needs the network, so it's ignored by default:
//!
//!   BSC_RPC_URL=https://... cargo test -p orvyn-mcp --test live -- --ignored --nocapture

use std::net::SocketAddr;

use alloy::primitives::{address, Address};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

// A large public BNB Chain wallet (Binance) holding BNB and USDT: only used as `from` in dry runs.
const RICH: &str = "0xF977814e90dA44bFA03b6295A0616a897441aceC";

async fn rpc(venue: &str, body: Value) -> (u16, Value) {
    let app = orvyn_mcp::router().layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));
    let req = Request::post(format!("/{venue}/mcp")).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// A tool call: Ok(data) or Err(error text).
async fn call(venue: &str, name: &str, args: Value) -> Result<Value, String> {
    let (_, v) = rpc(venue, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } })).await;
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    if v["result"]["isError"] == true { Err(text) } else { serde_json::from_str(&text).map_err(|_| text) }
}

struct Report(u32);
impl Report {
    fn check(&mut self, label: &str, pass: bool, extra: impl std::fmt::Display) {
        if !pass {
            self.0 += 1;
        }
        println!("{} {label} — {extra}", if pass { "PASS" } else { "FAIL" });
    }
}

fn show(r: &Result<Value, String>, f: impl Fn(&Value) -> String) -> String {
    match r {
        Ok(v) => f(v),
        Err(e) => e.clone(),
    }
}

/// Token addresses seen in recent logs of a contract (words that look like addresses).
async fn recent_addresses(contract: Address, blocks: u64) -> Vec<String> {
    let p = orvyn_mcp::chain::bsc();
    let head = p.get_block_number().await.unwrap();
    let logs = p.get_logs(&Filter::new().address(contract).from_block(head - blocks).to_block(head)).await.unwrap_or_default();
    let mut out: Vec<String> = vec![];
    for l in logs {
        let hex = alloy::primitives::hex::encode(&l.data().data);
        for w in hex.as_bytes().chunks(64).filter(|w| w.len() == 64) {
            let w = std::str::from_utf8(w).unwrap();
            if w.starts_with("000000000000000000000000") && !w.starts_with("0000000000000000000000000000000000000000") {
                let a = format!("0x{}", &w[24..]);
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
    }
    out
}

#[tokio::test]
#[ignore = "needs BNB Chain mainnet"]
async fn venues_live() {
    if std::env::var("BSC_RPC_URL").is_err() {
        // SAFETY: before any client exists, in this test binary only.
        unsafe { std::env::set_var("BSC_RPC_URL", "https://bsc-rpc.publicnode.com") };
    }
    let mut r = Report(0);

    // ---- The protocol over HTTP
    let (s, v) = rpc("pancakeswap", json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} })).await;
    let names: Vec<&str> = v["result"]["tools"].as_array().map(|t| t.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
    r.check("HTTP: tools/list", s == 200 && names == ["pancakeswap_quote", "pancakeswap_build_swap"], format!("{names:?}"));
    let (s, v) = rpc("flap", json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } })).await;
    r.check("HTTP: initialize", s == 200 && v["result"]["serverInfo"]["name"] == "orvyn-flap" && v["result"]["protocolVersion"] == "2025-06-18", v["result"]["serverInfo"].to_string());
    let (s, _) = rpc("nope", json!({})).await;
    r.check("HTTP: unknown venue is a 404", s == 404, s);
    let bad = call("pancakeswap", "pancakeswap_quote", json!({ "token_in": "BNB" })).await;
    r.check("refuses missing arguments", bad.as_ref().is_err_and(|e| e.contains("-32602")), show(&bad, |v| v.to_string()));

    // ---- PancakeSwap
    let q = call("pancakeswap", "pancakeswap_quote", json!({ "token_in": "BNB", "token_out": "USDT", "amount_in": "1" })).await;
    r.check("pancakeswap quote BNB→USDT", q.as_ref().is_ok_and(|v| v["amountOut"].as_str().and_then(|a| a.parse::<f64>().ok()).unwrap_or(0.0) > 100.0), show(&q, |v| format!("{} USDT via {}", v["amountOut"], v["route"])));
    for version in ["v3", "v2"] {
        let b = call("pancakeswap", "pancakeswap_build_swap", json!({ "token_in": "BNB", "token_out": "USDT", "amount_in": "0.5", "account": RICH, "version": version })).await;
        r.check(
            &format!("pancakeswap build BNB→USDT ({version}) dry-runs"),
            b.as_ref().is_ok_and(|v| v["calls"].as_array().unwrap().len() == 1 && v["simulation"]["status"] == "ok"),
            show(&b, |v| format!("{}; min {}", v["simulation"], v["quote"]["minAmountOut"])),
        );
    }
    let b = call("pancakeswap", "pancakeswap_build_swap", json!({ "token_in": "USDT", "token_out": "BNB", "amount_in": "100", "account": RICH })).await;
    r.check(
        "pancakeswap build USDT→BNB: approval first, both dry-run in order",
        b.as_ref().is_ok_and(|v| v["calls"].as_array().unwrap().len() == 2 && v["calls"][0]["description"].as_str().unwrap().starts_with("Approve 100 USDT") && v["simulation"]["status"] == "ok"),
        show(&b, |v| format!("{} | {}", v["calls"].as_array().unwrap().iter().map(|c| c["description"].as_str().unwrap()).collect::<Vec<_>>().join(" | "), v["simulation"])),
    );
    let b = call("pancakeswap", "pancakeswap_build_swap", json!({ "token_in": "BNB", "token_out": "USDT", "amount_in": "1", "account": "0x7a3f00000000000000000000000000000000c0de" })).await;
    r.check("refuses a swap the account can't cover", b.as_ref().is_err_and(|e| e.contains("less than")), show(&b, |v| v.to_string()));
    let b = call("pancakeswap", "pancakeswap_quote", json!({ "token_in": "BNB", "token_out": "WBNB", "amount_in": "1" })).await;
    r.check("refuses BNB→WBNB", b.is_err(), show(&b, |v| v.to_string()));
    let b = call("pancakeswap", "pancakeswap_quote", json!({ "token_in": "not-a-token", "token_out": "USDT", "amount_in": "1" })).await;
    r.check("refuses a bad token", b.is_err(), show(&b, |v| v.to_string()));
    let b = call("pancakeswap", "pancakeswap_build_swap", json!({ "token_in": "BNB", "token_out": "USDT", "amount_in": "1", "account": RICH, "slippage_bps": 5000 })).await;
    r.check("refuses slippage above the cap", b.is_err(), show(&b, |v| v.to_string()));

    // ---- Uniswap
    let b = call("uniswap", "uniswap_build_swap", json!({ "token_in": "BNB", "token_out": "USDT", "amount_in": "0.5", "account": RICH })).await;
    r.check("uniswap build BNB→USDT dry-runs", b.as_ref().is_ok_and(|v| v["simulation"]["status"] == "ok"), show(&b, |v| format!("{} via {}", v["simulation"], v["quote"]["route"])));
    let q = call("uniswap", "uniswap_quote", json!({ "token_in": "USDC", "token_out": "BNB", "amount_in": "100" })).await;
    r.check("uniswap quote USDC→BNB", q.as_ref().is_ok_and(|v| v["amountOut"].as_str().and_then(|a| a.parse::<f64>().ok()).unwrap_or(0.0) > 0.0), show(&q, |v| format!("{} BNB", v["amountOut"])));

    // ---- four.meme and Flap: live curve tokens from recent manager / portal logs
    let mut found = false;
    for token in recent_addresses(address!("5c952063c7fc8610FFDB798152D69F0B9550762b"), 100).await {
        let Ok(info) = call("fourmeme", "fourmeme_token_info", json!({ "token": token })).await else { continue };
        if info["graduated"] == true || info["quoteToken"] != "BNB" {
            continue;
        }
        let b = call("fourmeme", "fourmeme_build_buy", json!({ "token": token, "amount": "0.01", "account": RICH })).await;
        r.check(
            "four.meme build buy dry-runs",
            b.as_ref().is_ok_and(|v| v["calls"].as_array().unwrap().len() == 1 && v["simulation"]["status"] == "ok"),
            show(&b, |v| format!("{}; {}; curve {}%", v["simulation"], v["calls"][0]["description"], info["curveProgressPct"])),
        );
        let q = call("fourmeme", "fourmeme_quote", json!({ "token": token, "side": "sell", "amount": "1000000" })).await;
        r.check("four.meme quote sell", q.as_ref().is_ok_and(|v| v["receive"].as_str().unwrap_or("").contains("BNB")), show(&q, |v| v["receive"].to_string()));
        found = true;
        break;
    }
    r.check("found a live four.meme curve token", found, "");

    let mut found = false;
    for token in recent_addresses(address!("e2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0"), 400).await {
        let Ok(info) = call("flap", "flap_token_info", json!({ "token": token })).await else { continue };
        if info["tradingOnCurve"] != true || info["quoteToken"] != "BNB" {
            continue;
        }
        let b = call("flap", "flap_build_swap", json!({ "token": token, "side": "buy", "amount": "0.01", "account": RICH })).await;
        r.check(
            "flap build buy dry-runs",
            b.as_ref().is_ok_and(|v| v["calls"].as_array().unwrap().len() == 1 && v["simulation"]["status"] == "ok"),
            show(&b, |v| format!("{}; {}", v["simulation"], v["calls"][0]["description"])),
        );
        let circulating: f64 = info["circulatingSupply"].as_str().unwrap().parse().unwrap();
        let q = call("flap", "flap_quote", json!({ "token": token, "side": "sell", "amount": format!("{:.6}", circulating / 10.0) })).await;
        let too_much = call("flap", "flap_quote", json!({ "token": token, "side": "sell", "amount": format!("{}", (circulating * 2.0).ceil() as u128 + 1) })).await;
        r.check("flap refuses selling more than circulates", too_much.as_ref().is_err_and(|e| e.contains("circulation")), show(&too_much, |v| v.to_string()));
        r.check("flap quote sell", q.as_ref().is_ok_and(|v| v["receive"].as_str().unwrap_or("").contains("BNB")), show(&q, |v| v["receive"].to_string()));
        found = true;
        break;
    }
    r.check("found a live Flap curve token", found, "");

    assert_eq!(r.0, 0, "{} live checks failed", r.0);
}
