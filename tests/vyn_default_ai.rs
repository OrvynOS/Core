//! Vyn works out of the box on Orvyn AI: her settings are made on first use (Orvyn AI, Claude Haiku
//! 5.5, a built-in key that only Vyn's requests carry), and what that key spends comes from a free
//! monthly allowance before the person's Orvyn credits. Switching back to Orvyn AI needs no key.
mod support;

use orvyn_core::{credits, vyn};

#[tokio::test]
async fn vyn_runs_on_orvyn_ai_with_a_free_allowance() {
    support::test_env(&[]);
    let db = support::shared_db().await;
    unsafe { std::env::set_var("VYN_FREE_USD_PER_MONTH", "1") };
    let me = "0x4444444444444444444444444444444444444444";

    // First use: settings appear, on Orvyn AI with the default model.
    let s = vyn::get_settings(me).await.unwrap().unwrap();
    assert_eq!((s["aiProvider"].as_str(), s["aiModel"].as_str(), s["builtIn"].as_bool()), (Some("orvyn"), Some(vyn::DEFAULT_MODEL), Some(true)));
    let again = vyn::ensure_settings(me).await.unwrap();
    let keys: i64 = sqlx::query_scalar("select count(*) from inference_keys where account_id = $1 and purpose = 'vyn' and revoked_at is null").bind(me).fetch_one(db).await.unwrap();
    assert_eq!((keys, again.ai_provider.as_str()), (1, "orvyn"), "made once");

    // The built-in key is Vyn's: its purpose comes back with its account.
    let secret = orvyn_core::secrets::decrypt_secret(&again.ai_key_encrypted, &format!("vyn:{me}"), orvyn_core::secrets::SecretKind::AiKey).unwrap();
    assert_eq!(credits::key_owner(&secret).await.unwrap(), Some((me.to_string(), Some("vyn".to_string()))));

    // The allowance pays first ($1), then credits.
    assert_eq!(credits::vyn_allowance_left(me).await.unwrap(), 1_000_000);
    assert_eq!(credits::charge_vyn(me, 600_000, "anthropic/claude-haiku-5.5", 1000, 500).await.unwrap(), 0, "all free");
    assert_eq!(credits::charge_vyn(me, 600_000, "anthropic/claude-haiku-5.5", 1000, 500).await.unwrap(), 200_000, "$0.40 free, $0.20 from credits");
    assert_eq!(credits::vyn_allowance_left(me).await.unwrap(), 0);
    assert_eq!(credits::balance(me).await.unwrap(), -200_000);
    let ledger: i64 = sqlx::query_scalar("select coalesce(sum(micro_usd), 0)::bigint from credit_ledger where account_id = $1 and source = 'vyn'").bind(me).fetch_one(db).await.unwrap();
    assert_eq!(ledger, -200_000, "only the paid part is on the credits ledger");

    // Back to Orvyn AI from another provider: no key needed, a fresh built-in key is made.
    sqlx::query("update vyn_settings set ai_provider = 'anthropic' where account_id = $1").bind(me).execute(db).await.unwrap();
    let input: vyn::SettingsInput = serde_json::from_value(serde_json::json!({ "aiProvider": "orvyn", "aiModel": "anthropic/claude-sonnet-5.5" })).unwrap();
    let saved = vyn::save_settings(me, &input).await.unwrap().unwrap();
    assert_eq!((saved["aiProvider"].as_str(), saved["aiModel"].as_str()), (Some("orvyn"), Some("anthropic/claude-sonnet-5.5")));
    let live: i64 = sqlx::query_scalar("select count(*) from inference_keys where account_id = $1 and purpose = 'vyn' and revoked_at is null").bind(me).fetch_one(db).await.unwrap();
    assert_eq!(live, 1, "the old built-in key is revoked");
    let bad: vyn::SettingsInput = serde_json::from_value(serde_json::json!({ "aiProvider": "orvyn", "aiModel": "made-up/model" })).unwrap();
    assert!(vyn::save_settings(me, &bad).await.is_err(), "only models Orvyn AI serves");
}
