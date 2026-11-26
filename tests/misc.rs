//! Formats that must match the TypeScript core byte for byte (vectors/misc.json): OrvynID claims,
//! Flap launch calldata, plugin settings, handles, GoPlus assessments, tax rules.
mod support;

use alloy::primitives::{B256, U256};
use orvyn_core::bliss::settings::*;
use orvyn_core::data::goplus::{assess_token, summarize_security, DEFAULT_POLICY};
use orvyn_core::identity::{claim_message, Claim};
use orvyn_core::launchpad::{flap_launch_call, validate_tax, TokenTax};
use orvyn_core::public::validate_handle;
use serde_json::Value;

fn v() -> Value {
    support::test_env(&[]);
    serde_json::from_str(include_str!("vectors/misc.json")).unwrap()
}

#[test]
fn orvyn_id_claims_sign_the_same_bytes() {
    let v = v();
    let claim: Claim = serde_json::from_value(v["claim"].clone()).unwrap();
    assert_eq!(claim_message(&claim), v["claimMessage"].as_str().unwrap());
}

#[test]
fn flap_launch_calldata_matches() {
    let v = v();
    let tax: TokenTax = serde_json::from_value(v["launch"]["tax"].clone()).unwrap();
    let salt: B256 = v["launch"]["salt"].as_str().unwrap().parse().unwrap();
    let data = flap_launch_call("Nova", "NOVA", "bafyCID", &tax, "0x3333333333333333333333333333333333333333".parse().unwrap(), U256::from(10u64).pow(U256::from(16u8)), salt).unwrap();
    assert_eq!(data.to_string(), v["launch"]["data"].as_str().unwrap());
}

#[test]
fn settings_parse_like_zod() {
    let v = v();
    let fields = vec![
        number("n", "N", None, 5.0, 0.0, 10.0, Some(1.0), None),
        number("f", "F", None, 0.5, 0.0, 1.0, None, None),
        boolean("b", "B", None, true),
        text("t", "T", None, "hi", 5, false),
        list("l", "L", None, vec!["A"], 3, true),
        select("s", "S", None, "x", &[("x", "X"), ("y", "Y")]),
    ];
    assert_eq!(js(&serde_json::to_value(&fields).unwrap()), js(&v["fields"]));
    for case in v["parsed"].as_array().unwrap() {
        let got = parse_settings(&fields, &case["input"]);
        match case.get("ok") {
            Some(want) => assert_eq!(js(&Value::Object(got.unwrap())), js(want), "{}", case["input"]),
            None => assert!(got.is_err(), "{} should be refused", case["input"]),
        }
    }
}

#[test]
fn handles_goplus_and_taxes() {
    let v = v();
    for h in v["handles"].as_array().unwrap() {
        assert_eq!(validate_handle(h["h"].as_str().unwrap()).as_deref(), h["error"].as_str(), "{}", h["h"]);
    }
    for r in v["reports"].as_array().unwrap() {
        let report = r["report"].as_object().map(|_| &r["report"]);
        assert_eq!(js(&serde_json::to_value(assess_token(report, DEFAULT_POLICY)).unwrap()), js(&r["assessment"]), "{}", r["report"]);
        let mut summary = summarize_security(report);
        let mut want = r["summary"].clone();
        // Flag order follows object key order on both sides; compare as sets.
        for s in [&mut summary, &mut want] {
            if let Some(a) = s.get_mut("risk_flags").and_then(Value::as_array_mut) {
                a.sort_by(|x, y| x.as_str().cmp(&y.as_str()));
            }
        }
        assert_eq!(js(&summary), js(&want));
    }
    for t in v["taxes"].as_array().unwrap() {
        let tax: TokenTax = serde_json::from_value(t["tax"].clone()).unwrap();
        match t.get("error") {
            None => assert!(validate_tax(Some(&tax)).is_ok()),
            Some(e) => assert_eq!(validate_tax(Some(&tax)).unwrap_err().message, e.as_str().unwrap()),
        }
    }
}

/// JSON as JavaScript prints it (5 and 5.0 are the same number there).
fn js(v: &Value) -> String {
    orvyn_core::environment::canonical_json(v)
}

/// The whole first-party plugin library, entry by entry: metadata, settings and defaults as the
/// TypeScript core lists them (vectors/catalog.json). Plugins added since the port are named here.
#[test]
fn the_plugin_catalog_matches() {
    support::test_env(&[]);
    const ADDED_SINCE: &[&str] = &["binance"];
    let want: Vec<Value> = serde_json::from_str(include_str!("vectors/catalog.json")).unwrap();
    let all = orvyn_core::bliss::bliss_catalog();
    let ids = |l: &[Value]| l.iter().map(|p| p["id"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    let added: Vec<String> = ids(&all).into_iter().filter(|id| !ids(&want).contains(id)).collect();
    assert_eq!(added, ADDED_SINCE, "plugins added since the TypeScript core");
    let got: Vec<Value> = all.into_iter().filter(|p| !ADDED_SINCE.contains(&p["id"].as_str().unwrap_or(""))).collect();
    assert_eq!(ids(&got), ids(&want), "same plugins, same order");
    for (g, w) in got.iter().zip(&want) {
        for key in ["name", "version", "author", "category", "summary", "description", "permissions", "settings", "defaults", "hasFeeShare", "hasAutopilot", "thirdParty"] {
            let canon = orvyn_core::environment::canonical_json;
            assert_eq!(canon(&g[key]), canon(&w[key]), "{} {key}", w["id"]);
        }
    }
}

/// Third-party MCP URLs in production: public https hosts only (the rest of the third-party plugin
/// checks are in external_plugins.rs, which allows local servers).
#[tokio::test]
async fn mcp_urls_must_be_public_https() {
    support::test_env(&[]);
    use orvyn_core::bliss::external::{assert_safe_url, url_host};
    for bad in ["http://example.com/mcp", "https://127.0.0.1/mcp", "https://localhost/mcp", "https://169.254.169.254/latest", "https://10.1.2.3/mcp", "https://[::1]/mcp", "https://user:pw@example.com/mcp", "notaurl"] {
        assert!(assert_safe_url(bad).await.is_err(), "refuses {bad}");
    }
    assert!(assert_safe_url("https://1.1.1.1/mcp").await.is_ok(), "accepts a public https host");
    // As the WHATWG URL `host`: the port only when it isn't the default.
    assert_eq!((url_host("https://mcp.example.com/x"), url_host("https://mcp.example.com:443/x"), url_host("http://127.0.0.1:8443/mcp")), ("mcp.example.com".into(), "mcp.example.com".into(), "127.0.0.1:8443".into()));
}
