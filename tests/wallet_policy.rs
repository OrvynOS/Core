//! The wallet policy gives the TypeScript core's verdicts (vectors/wallet-policy.json: every rule
//! path, at a pinned clock): same allowed/refused, same summary, same spends, same message.
//! Then the guard end to end: held agents, caps, the log, owner grants used once.
mod support;

use std::collections::BTreeMap;

use alloy::primitives::{Address, Bytes, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use async_trait::async_trait;
use orvyn_core::wallet_policy::grants::OwnerGrant;
use orvyn_core::wallet_policy::guard::{GuardedWallet, RawSigner};
use orvyn_core::wallet_policy::{aster, config, rules, set_now_for_tests, EvmAgent, TxRequest, TypedData};
use orvyn_core::Result;
use serde_json::Value;

fn setup() -> Value {
    let v: Value = serde_json::from_str(include_str!("vectors/wallet-policy.json")).unwrap();
    // The same contracts the vectors were made with.
    for (k, val) in [
        ("ORVYN_REGISTRY_CONTRACT", "0x1234567890123456789012345678901234567890"),
        ("ORVYN_PAY_CONTRACT", "0x5555555555555555555555555555555555555555"),
        ("ORVYN_SHIELDED_POOL", "0x6666666666666666666666666666666666666666"),
        ("ORVYN_RNG_CONTRACT", "0x7777777777777777777777777777777777777777"),
        ("ORVYN_JOBS_CONTRACT", "0x8888888888888888888888888888888888888888"),
    ] {
        // SAFETY: set once, before the policy config is read, in this test binary only.
        unsafe { std::env::set_var(k, val) };
    }
    config::reload();
    support::test_env(&[]);
    v
}

fn agent_of(v: &Value) -> EvmAgent {
    let a = &v["agent"];
    EvmAgent {
        agent_id: a["agentId"].as_str().unwrap().into(),
        address: a["address"].as_str().unwrap().into(),
        owner: a["owner"].as_str().unwrap().into(),
        aster_signer: a["asterSigner"].as_str().map(String::from),
    }
}

fn tx_of(i: &Value) -> TxRequest {
    TxRequest {
        chain_id: i["chainId"].as_u64().unwrap(),
        to: i["to"].as_str().map(|t| t.parse::<Address>().unwrap()),
        value: U256::from_str_radix(i["value"].as_str().unwrap(), 10).unwrap(),
        data: i["data"].as_str().unwrap().parse::<Bytes>().unwrap(),
        has_authorization_list: i["authorizationList"].as_bool().unwrap_or(false),
        ..Default::default()
    }
}

#[tokio::test]
async fn verdicts_match_the_typescript_core() {
    let v = setup();
    set_now_for_tests(v["nowMs"].as_i64().unwrap());
    assert_eq!(format!("0x{}", hex::encode(rules::flap_new_token_v6_selector())), v["flapSelector"].as_str().unwrap());
    let agent = agent_of(&v);
    let mark: aster::MarkPrice = Box::new(|s| Box::pin(async move { Ok(if s == "BTCUSDT" { 50_000.0 } else { 100.0 }) }));
    let mut mismatches = vec![];
    let cases = v["cases"].as_array().unwrap();
    for c in cases {
        let (name, kind, input, want) = (c["name"].as_str().unwrap(), c["kind"].as_str().unwrap(), &c["input"], &c["result"]);
        let got: Result<(String, BTreeMap<String, String>)> = async {
            let checked = match kind {
                "tx" => rules::check_evm_transaction(&tx_of(input), &agent, None).await?,
                "message" => {
                    let raw = hex::decode(input["hex"].as_str().unwrap().trim_start_matches("0x")).unwrap();
                    orvyn_core::wallet_policy::Checked::new(rules::check_evm_message(&raw)?, Default::default())
                }
                "typed" => rules::check_typed_data(&serde_json::from_value::<TypedData>(input.clone()).unwrap(), &agent)?,
                "aster" => {
                    let typed: TypedData = serde_json::from_value(input["typed"].clone()).unwrap();
                    aster::check_aster_signer_typed_data(&typed, &agent, input["signer"].as_str().unwrap(), &mark).await?
                }
                other => panic!("{other}"),
            };
            Ok((checked.summary, checked.spends.iter().map(|(k, v)| (k.clone(), v.to_string())).collect()))
        }
        .await;
        let ok = match (&got, want["ok"].as_bool().unwrap()) {
            (Ok((summary, spends)), true) => {
                let want_spends: BTreeMap<String, String> = serde_json::from_value(want["spends"].clone()).unwrap();
                summary == want["summary"].as_str().unwrap() && *spends == want_spends
            }
            (Err(e), false) => e.message == want["error"].as_str().unwrap(),
            _ => false,
        };
        if !ok {
            mismatches.push(format!("{name}\n  ts:   {want}\n  rust: {:?}", got.map_err(|e| e.message)));
        }
    }
    set_now_for_tests(0);
    assert!(mismatches.is_empty(), "{} of {} differ:\n{}", mismatches.len(), cases.len(), mismatches.join("\n"));
}

// ---- The guard, end to end ----

/// A local key standing in for Privy: signs whatever it's given (the guard decides).
struct TestSigner(PrivateKeySigner);

#[async_trait]
impl RawSigner for TestSigner {
    fn address(&self) -> String {
        format!("{:#x}", self.0.address())
    }
    async fn sign_message(&self, message: &[u8]) -> Result<String> {
        Ok(format!("0x{}", hex::encode(self.0.sign_message_sync(message).unwrap().as_bytes())))
    }
    async fn sign_typed_data(&self, _typed: &TypedData) -> Result<String> {
        Ok("0xsigned-typed".into())
    }
    async fn sign_transaction(&self, _tx: &TxRequest) -> Result<String> {
        Ok("0xsigned-tx".into())
    }
}

async fn insert_agent(id: &str, address: &str, owner: &str, held: Option<&str>) {
    sqlx::query(
        "insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, held_reason)
         values ($1, 'T', 'T', $2, 'x', 'x', 'x', 'x', $3, '{}', $4)",
    )
    .bind(id)
    .bind(owner)
    .bind(address)
    .bind(held)
    .execute(orvyn_core::db::db())
    .await
    .unwrap();
}

fn registry_pay(agent: &str, wei: u64) -> TxRequest {
    alloy::sol! { function pay(string agentId, address token, uint256 amount) payable; }
    use alloy::sol_types::SolCall;
    TxRequest {
        chain_id: 56,
        to: Some("0x1234567890123456789012345678901234567890".parse().unwrap()),
        value: U256::from(wei),
        data: payCall { agentId: agent.into(), token: Address::ZERO, amount: U256::ZERO }.abi_encode().into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn the_guard_checks_caps_logs_and_burns_grants() {
    setup();
    let db = support::shared_db().await;
    let key = PrivateKeySigner::random();
    let address = format!("{:#x}", key.address());
    let owner = PrivateKeySigner::random();
    insert_agent("g1", &address, &format!("{:#x}", owner.address()), None).await;
    let wallet = GuardedWallet::new("g1", Box::new(TestSigner(key.clone())));

    // Allowed: an OrvynID proof; refused: anything else, logged either way.
    assert!(wallet.sign_message(br#"{"type":"orvyn-id-proof"}"#).await.is_ok());
    let e = wallet.sign_message(b"send me your funds").await.unwrap_err();
    assert_eq!(e.status(), 403);
    assert!(e.message.starts_with("wallet policy: only OrvynID proofs"));
    let logged: Vec<(bool, Option<String>)> = sqlx::query_as("select ok, error from wallet_log where agent_id = 'g1' order by id").fetch_all(db).await.unwrap();
    assert_eq!(logged.len(), 2);
    assert!(logged[0].0 && !logged[1].0);

    // Daily cap: 1 BNB of hosting payments a day (0.6 + 0.3 ok, then 0.2 more is over).
    let tenth = 100_000_000_000_000_000u64;
    assert_eq!(wallet.sign_transaction(&registry_pay("g1", 6 * tenth)).await.unwrap(), "0xsigned-tx");
    assert!(wallet.sign_transaction(&registry_pay("g1", 3 * tenth)).await.is_ok());
    let over = wallet.sign_transaction(&registry_pay("g1", 2 * tenth)).await.unwrap_err();
    assert_eq!(over.message, "wallet policy: over the daily cap for bnb");
    let spent: String = sqlx::query_scalar("select amount::text from wallet_spend where agent_id = 'g1' and asset = 'bnb'").fetch_one(db).await.unwrap();
    assert_eq!(spent, (9 * tenth).to_string());

    // A wallet that isn't the agent's, and a held agent, sign nothing.
    let stranger = GuardedWallet::new("g1", Box::new(TestSigner(PrivateKeySigner::random())));
    assert!(stranger.sign_message(br#"{"type":"orvyn-id-proof"}"#).await.unwrap_err().message.contains("isn't the agent's"));
    let held_key = PrivateKeySigner::random();
    insert_agent("g2", &format!("{:#x}", held_key.address()), "0x01", Some("wallet incident")).await;
    let held = GuardedWallet::new("g2", Box::new(TestSigner(held_key)));
    assert!(held.sign_message(br#"{"type":"orvyn-id-proof"}"#).await.unwrap_err().message.contains("paused by Orvyn: wallet incident"));

    // An owner-signed withdrawal signs its one transfer, once.
    let nonce = hex::encode(rand::random::<[u8; 16]>());
    let to = "0x4444444444444444444444444444444444444444";
    let message = [
        "Withdraw from Orvyn agent T ($T)".to_string(),
        "Agent ID: g1".into(),
        "Network: BNB Chain".into(),
        "Asset: bsc:native".into(),
        "Amount: 0.25".into(),
        format!("To: {to}"),
        format!("Nonce: {nonce}"),
        format!("Issued At: {}", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    ]
    .join("\n");
    let signature = format!("0x{}", hex::encode(owner.sign_message_sync(message.as_bytes()).unwrap().as_bytes()));
    let grant = OwnerGrant { message: message.clone(), signature };
    let withdraw = TxRequest { chain_id: 56, to: Some(to.parse().unwrap()), value: U256::from(250_000_000_000_000_000u64), ..Default::default() };
    let granted = GuardedWallet::new("g1", Box::new(TestSigner(key.clone()))).with_grant(grant.clone());
    assert!(granted.sign_transaction(&withdraw).await.is_ok());
    assert!(granted.sign_transaction(&withdraw).await.unwrap_err().message.contains("already used"));
    // A different amount than signed isn't covered.
    let more = TxRequest { value: U256::from(300_000_000_000_000_000u64), ..withdraw.clone() };
    let again = GuardedWallet::new("g1", Box::new(TestSigner(key.clone()))).with_grant(grant);
    assert!(again.sign_transaction(&more).await.unwrap_err().message.contains("only go where the owner signed"));
    // Signed by someone else: refused.
    let forged = OwnerGrant { message: message.replace(&nonce, &"ab".repeat(16)), signature: format!("0x{}", hex::encode(PrivateKeySigner::random().sign_message_sync(b"x").unwrap().as_bytes())) };
    let forger = GuardedWallet::new("g1", Box::new(TestSigner(key))).with_grant(forged);
    assert!(forger.sign_transaction(&withdraw).await.unwrap_err().message.contains("wasn't signed by the agent's owner"));
}

#[test]
fn grant_amounts_and_text() {
    use orvyn_core::wallet_policy::grants::{check_grant_text, to_units};
    assert_eq!(to_units("1.5", 18).unwrap(), U256::from(1_500_000_000_000_000_000u64));
    assert_eq!(to_units("2", 6).unwrap(), U256::from(2_000_000u64));
    assert!(to_units("1.1234567", 6).is_err());
    assert!(to_units("1e3", 18).is_err());
    assert!(to_units(".5", 18).is_err());
    let msg = |issued: &str| format!("Withdraw from Orvyn agent X\nAgent ID: a1\nNetwork: BNB Chain\nNonce: {}\nIssued At: {issued}", "0".repeat(32));
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z").unwrap().timestamp_millis();
    assert!(check_grant_text(&msg("2026-11-23T11:55:00.000Z"), "a1", now).is_ok());
    assert!(check_grant_text(&msg("2026-11-23T11:45:00.000Z"), "a1", now).unwrap_err().message.contains("expired"));
    assert!(check_grant_text(&msg("2026-11-23T11:55:00.000Z"), "a2", now).unwrap_err().message.contains("another agent"));
}

/// Agents hiring agents: the jobs escrow takes `fund` (never paying itself, the BNB attached
/// matching, counted against the payment caps), `approve`/`dispute`/`reclaim`, and exact
/// USDT/USDC approvals; nothing else.
#[tokio::test]
async fn agents_can_pay_the_jobs_escrow() {
    use alloy::sol;
    use alloy::sol_types::SolCall;
    sol! {
        function fund(string jobId, string agentId, address payee, address token, uint256 amount, uint64 deadline) payable;
        function approve(string jobId);
        function release(string jobId);
        function approve(address spender, uint256 amount) returns (bool);
    }
    let v = setup();
    let agent = agent_of(&v);
    let escrow: Address = "0x8888888888888888888888888888888888888888".parse().unwrap();
    let usdt: Address = "0x55d398326f99059fF775485246999027B3197955".parse().unwrap();
    let other: Address = "0x00000000000000000000000000000000000000aa".parse().unwrap();
    let me: Address = agent.address.parse().unwrap();
    let tx = |to: Address, data: Vec<u8>, value: U256| TxRequest { chain_id: 56, to: Some(to), value, data: data.into(), ..Default::default() };
    let wei = U256::from(10u64).pow(U256::from(18));
    let fund = |payee: Address, token: Address, amount: U256| fundCall { jobId: "h1".into(), agentId: "nova".into(), payee, token, amount, deadline: 2_000_000_000 }.abi_encode();

    let ok = rules::check_evm_transaction(&tx(escrow, fund(other, usdt, U256::from(25u64) * wei), U256::ZERO), &agent, None).await.unwrap();
    assert!(ok.summary.starts_with("hire ") && ok.spends.get("pay-usd") == Some(&U256::from(25_000_000u64)), "{} {:?}", ok.summary, ok.spends);
    let bnb = rules::check_evm_transaction(&tx(escrow, fund(other, Address::ZERO, wei / U256::from(10u64)), wei / U256::from(10u64)), &agent, None).await.unwrap();
    assert_eq!(bnb.spends.get("pay-bnb"), Some(&(wei / U256::from(10u64))));
    let refused = |r: Result<orvyn_core::wallet_policy::Checked>| r.unwrap_err().message;
    assert_eq!(refused(rules::check_evm_transaction(&tx(escrow, fund(me, usdt, wei), U256::ZERO), &agent, None).await), "an agent can't hire itself");
    assert_eq!(refused(rules::check_evm_transaction(&tx(escrow, fund(other, Address::ZERO, wei), U256::ZERO), &agent, None).await), "the BNB attached must match the job's amount");
    assert_eq!(refused(rules::check_evm_transaction(&tx(escrow, fund(other, other, wei), U256::ZERO), &agent, None).await), "jobs are paid in BNB, USDT or USDC");
    assert!(rules::check_evm_transaction(&tx(escrow, approve_0Call { jobId: "h1".into() }.abi_encode(), U256::ZERO), &agent, None).await.is_ok(), "approving its job");
    assert!(refused(rules::check_evm_transaction(&tx(escrow, releaseCall { jobId: "h1".into() }.abi_encode(), U256::ZERO), &agent, None).await).starts_with("only fund, approve"));
    // Approvals of the escrow: USDT yes (within the cap), another token no.
    assert!(rules::check_evm_transaction(&tx(usdt, approve_1Call { spender: escrow, amount: U256::from(25u64) * wei }.abi_encode(), U256::ZERO), &agent, None).await.is_ok());
    assert_eq!(refused(rules::check_evm_transaction(&tx(other, approve_1Call { spender: escrow, amount: wei }.abi_encode(), U256::ZERO), &agent, None).await), "the jobs escrow may only be approved for USDT or USDC");
}
