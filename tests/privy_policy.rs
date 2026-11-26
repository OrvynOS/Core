//! Orvyn's Privy wallet policies, rule for rule (vectors/privy-policy.json: the TypeScript core's, with the
//! rule names shortened and the transfer rule matched on its amount, as Privy's API requires;
//! made with the same contract addresses as the wallet-policy vectors).

use orvyn_core::wallet_policy::config;
use orvyn_core::wallet_policy::privy_policy::{agent_policy_rules, aster_signer_policy_rules};
use serde_json::{json, Value};

#[test]
fn policies_are_the_typescript_cores() {
    for (k, val) in [
        ("ORVYN_REGISTRY_CONTRACT", "0x1234567890123456789012345678901234567890"),
        ("ORVYN_PAY_CONTRACT", "0x5555555555555555555555555555555555555555"),
        ("ORVYN_SHIELDED_POOL", "0x6666666666666666666666666666666666666666"),
        ("ORVYN_RNG_CONTRACT", "0x7777777777777777777777777777777777777777"),
    ] {
        // SAFETY: set before the policy config is read, in this test binary only.
        unsafe { std::env::set_var(k, val) };
    }
    config::reload();
    let v: Value = serde_json::from_str(include_str!("vectors/privy-policy.json")).unwrap();
    let ours = json!({ "agent": agent_policy_rules(), "asterSigner": aster_signer_policy_rules() });
    for (i, (a, b)) in ours["agent"].as_array().unwrap().iter().zip(v["agent"].as_array().unwrap()).enumerate() {
        assert_eq!(a, b, "agent rule {i}");
    }
    assert_eq!(ours, v);
}
