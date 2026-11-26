//! Live checks against mainnet services, with throwaway wallets (no funds, nothing sent). Ignored by
//! default: `cargo test -p orvyn-core --test live -- --ignored --nocapture`.
//!
//! - Aster Futures API V3: market data and step rounding; approving an API wallet with the main
//!   wallet's signature (an approval signed by another key is refused); signed reads by the approved
//!   and an unapproved wallet. A fresh account hasn't deposited and Aster checks the deposit before
//!   the request signature, so request signing itself is proven only on a funded account: set
//!   ASTER_CHECK_KEY / ASTER_CHECK_API_KEY (0x keys of a funded main wallet and its approved API
//!   wallet) for a real signed balance read.
//! - Flap: a vanity salt is found, and newTokenV6 is simulated (eth_call) on the live Portal with an
//!   agent's tax settings: it creates the token exactly at the predicted 7777 address; bad settings
//!   are refused; a live token's curve reads back.
mod support;

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use orvyn_core::aster;
use orvyn_core::evm;
use orvyn_core::launchpad::{find_vanity_salt, flap_curve_info, flap_launch_call, TokenTax};
use orvyn_core::wallet_policy::guard::{GuardedAsterWallet, GuardedWallet, RawSigner};
use orvyn_core::wallet_policy::{TxRequest, TypedData};
use orvyn_core::wallets::LocalSigner;
use orvyn_core::Result;

struct Checks(Vec<String>);

impl Checks {
    fn ok(&mut self, name: &str, pass: bool, detail: impl std::fmt::Display) {
        println!("{} {name}: {detail}", if pass { "ok  " } else { "FAIL" });
        if !pass {
            self.0.push(name.into());
        }
    }
    fn done(self, what: &str) {
        assert!(self.0.is_empty(), "{what}: failed {:?}", self.0);
    }
}

fn key_from_env(name: &str) -> PrivateKeySigner {
    std::env::var(name).ok().and_then(|k| k.parse().ok()).unwrap_or_else(PrivateKeySigner::random)
}

/// Claims one address but signs with another key: a forged approval.
struct Impostor {
    claims: String,
    signs: LocalSigner,
}

#[async_trait]
impl RawSigner for Impostor {
    fn address(&self) -> String {
        self.claims.clone()
    }
    async fn sign_message(&self, message: &[u8]) -> Result<String> {
        self.signs.sign_message(message).await
    }
    async fn sign_typed_data(&self, typed: &TypedData) -> Result<String> {
        self.signs.sign_typed_data(typed).await
    }
    async fn sign_transaction(&self, tx: &TxRequest) -> Result<String> {
        self.signs.sign_transaction(tx).await
    }
}

#[tokio::test]
#[ignore = "live: Aster mainnet API"]
async fn aster_api() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    let mut c = Checks(vec![]);
    let symbols = aster::aster_symbols().await.unwrap();
    let btc = symbols.get("BTCUSDT");
    c.ok("exchange info", btc.is_some_and(|b| b.step_size > 0.0 && b.tick_size > 0.0), format!("{} symbols; BTCUSDT {btc:?}", symbols.len()));
    let mark = aster::aster_mark("BTCUSDT").await.unwrap();
    c.ok("mark price", mark["markPrice"].as_f64().unwrap_or(0.0) > 0.0, &mark);
    c.ok("step rounding", aster::to_step(0.0123456, 0.001) == "0.012" && aster::to_step(5.0, 1.0) == "5" && aster::to_step(0.3, 0.1) == "0.3", "");
    let ticker = aster::aster_public("/fapi/v3/ticker/24hr", &vec![("symbol".into(), "ETHUSDT".into())]).await.unwrap();
    c.ok("24h ticker", ticker["lastPrice"].as_str().and_then(|p| p.parse::<f64>().ok()).unwrap_or(0.0) > 0.0, &ticker["lastPrice"]);

    let (main_key, api_key) = (key_from_env("ASTER_CHECK_KEY"), key_from_env("ASTER_CHECK_API_KEY"));
    let main_address = format!("{:#x}", main_key.address());
    let api_address = format!("{:#x}", api_key.address());
    // The agent, with its Aster API wallet on record: the wallet policy only lets an agent approve
    // and sign with its own (every signature here goes through the policy, as in production).
    sqlx::query("insert into agents (id, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('aster-check', 'A', 'A', '0x1', 'x', 'x', 'x', 'x', $1, '{}', 'live')")
        .bind(&main_address)
        .execute(db)
        .await
        .unwrap();
    let record_signer = |address: String| async move {
        sqlx::query("insert into agent_plugins (agent_id, plugin_id, config, state) values ('aster-check', 'aster', '{}', $1) on conflict (agent_id, plugin_id) do update set state = excluded.state")
            .bind(serde_json::json!({ "asterSigner": { "address": address } }))
            .execute(db)
            .await
            .unwrap();
    };
    record_signer(api_address.clone()).await;
    let main = GuardedWallet::new("aster-check", Box::new(LocalSigner(main_key.clone())));
    let api = GuardedAsterWallet::new("aster-check", Box::new(LocalSigner(api_key.clone())));
    let expires = chrono::Utc::now().timestamp_millis() + 30 * 86_400_000;
    let approved = aster::aster_approve_api_wallet(&main, &api_address, expires, "orvyncheck").await;
    c.ok("approve API wallet (main wallet signature)", approved.is_ok(), format!("{approved:?}"));

    let read = |signer: GuardedAsterWallet| {
        let user = main_address.clone();
        async move { aster::aster_signed(reqwest::Method::GET, "/fapi/v3/balance", vec![], &user, &signer).await }
    };
    let good = read(api).await;
    // A wallet Aster never approved (on record here, so the policy lets it through to Aster).
    let stranger_key = PrivateKeySigner::random();
    record_signer(format!("{:#x}", stranger_key.address())).await;
    let bad = read(GuardedAsterWallet::new("aster-check", Box::new(LocalSigner(stranger_key)))).await;
    record_signer(api_address.clone()).await;
    let message = |r: &Result<serde_json::Value>| r.as_ref().err().map(|e| e.message.clone()).unwrap_or_default();
    println!("     approved wallet: {}", if good.is_ok() { "balance read".to_string() } else { message(&good) });
    println!("     unapproved wallet: {}", message(&bad));
    c.ok("an unapproved wallet is refused", bad.is_err(), message(&bad));
    // Unfunded, the approved wallet gets the deposit error, not the unknown-signer one.
    c.ok("the approved wallet is recognised (signed read, or Aster asking for a deposit first)", good.is_ok() || message(&good) != message(&bad), message(&good));

    let forged = GuardedWallet::new("aster-check", Box::new(Impostor { claims: main_address.clone(), signs: LocalSigner(PrivateKeySigner::random()) }));
    let r = aster::aster_approve_api_wallet(&forged, &api_address, chrono::Utc::now().timestamp_millis() + 86_400_000, "forged").await;
    c.ok("an approval signed by another key is refused", r.is_err(), message(&r));
    c.done("Aster");
}

#[tokio::test]
#[ignore = "live: BNB Chain mainnet (Flap Portal)"]
async fn flap_launch_simulation() {
    support::test_env(&[]);
    let mut c = Checks(vec![]);
    let creator = PrivateKeySigner::random().address();
    let started = std::time::Instant::now();
    let (salt, predicted) = find_vanity_salt(B256::from(rand::random::<[u8; 32]>()), "7777", 2_000_000).unwrap();
    c.ok("vanity salt found", format!("{predicted:x}").ends_with("7777"), format!("{predicted} in {} ms", started.elapsed().as_millis()));
    let portal: Address = "0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0".parse().unwrap();
    let code = evm::bsc().get_code_at(portal).await.unwrap();
    c.ok("Portal has code", !code.is_empty(), format!("{} bytes", code.len()));

    let tax = TokenTax { fee_rate: 5, recipient_rate: 60, burn_rate: 10, divide_rate: 20, liquidity_rate: 10, min_sharing: 100_000 };
    let meta = "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku";
    // Simulate newTokenV6: Ok(created token) or Err(why it was refused).
    let simulate = |tax: TokenTax, salt: B256, first_buy: U256, from: Address| async move {
        let data = flap_launch_call("Orvyn Check", "OCHK", meta, &tax, creator, first_buy, salt).map_err(|e| format!("before the chain: {}", e.message))?;
        let tx = TransactionRequest::default().with_from(from).with_to(portal).with_input(data).with_value(first_buy);
        let out = evm::bsc().call(tx).await.map_err(|e| format!("reverted: {}", e.to_string().chars().take(160).collect::<String>()))?;
        Ok::<Address, String>(Address::from_slice(&out[out.len() - 20..]))
    };
    let r = simulate(tax.clone(), salt, U256::ZERO, creator).await;
    c.ok("a 5% tax token launches (60% to the agent, 10% burn, 20% holders, 10% LP)", r.as_ref().is_ok_and(|t| *t == predicted), format!("{r:?}"));
    let funded: Address = "0x8894E0a0c962CB723c1976a4421c95949bE2D4E3".parse().unwrap();
    let r = simulate(tax.clone(), salt, U256::from(10u64).pow(U256::from(16u8)), funded).await;
    c.ok("with a 0.01 BNB first buy", r.as_ref().is_ok_and(|t| *t == predicted), format!("{r:?}"));
    let r = simulate(tax.clone(), B256::repeat_byte(0x11), U256::ZERO, creator).await;
    c.ok("a salt whose address doesn't end in 7777 is refused", r.is_err(), format!("{r:?}"));
    let r = simulate(TokenTax { recipient_rate: 50, ..tax }, salt, U256::ZERO, creator).await;
    c.ok("a tax split not adding up to 100% is refused before reaching the chain", r.as_ref().is_err_and(|e| e.starts_with("before the chain")), format!("{r:?}"));

    // A live token on the curve, from recent Portal logs: its curve reads back.
    let head = evm::bsc().get_block_number().await.unwrap();
    let filter = alloy::rpc::types::Filter::new().address(portal).from_block(head - 200).to_block(head);
    let logs = evm::bsc().get_logs(&filter).await.unwrap_or_default();
    let mut candidates: Vec<String> = vec![];
    for l in &logs {
        for w in l.data().data.chunks(32).filter(|w| w.len() == 32) {
            let word = hex::encode(w);
            if word.starts_with(&"0".repeat(24)) && (word.ends_with("7777") || word.ends_with("8888")) && !candidates.contains(&word[24..].to_string()) {
                candidates.push(word[24..].to_string());
            }
        }
    }
    let mut found = None;
    for token in &candidates {
        if let Ok(info) = flap_curve_info(&format!("0x{token}")).await {
            found = Some((token.clone(), info));
            break;
        }
    }
    c.ok("a live Flap token's curve reads back", found.is_some(), match &found {
        Some((t, i)) => format!("0x{t}: progress {:?}%, graduated {}", i.progress_pct, i.graduated),
        None => format!("{} candidates in the last 200 blocks", candidates.len()),
    });
    c.done("Flap");
}
