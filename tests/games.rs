//! Games against the TypeScript core (vectors/games.json): slugs, referenced assets, the engine's
//! checks on an example game (natively here, as WebAssembly there), leaderboards, world checks,
//! play tokens and payment calls; and a game's life on a throwaway database.
mod support;

use orvyn_core::db::Game;
use orvyn_core::games::{self, engine, scores, worlds};
use serde_json::{json, Value};

fn v() -> Value {
    support::test_env(&[]);
    serde_json::from_str(include_str!("vectors/games.json")).unwrap()
}

fn canon(v: &Value) -> String {
    orvyn_core::environment::canonical_json(v)
}

fn game_row(id: &str, access: Value) -> Game {
    let now = chrono::Utc::now();
    Game {
        id: id.into(),
        slug: "s".into(),
        title: "T".into(),
        description: String::new(),
        creator_account_id: "0x1111111111111111111111111111111111111111".into(),
        agent_id: None,
        publish_as_agent: false,
        hire_id: None,
        status: "draft".into(),
        access,
        draft: json!({}),
        problems: json!([]),
        version: 0,
        plays: 0,
        thumbnail: None,
        thumbnail_version: None,
        published_at: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn slugs_assets_boards_and_worlds_match() {
    let v = v();
    for s in v["slugs"].as_array().unwrap() {
        assert_eq!(games::slugify(s[0].as_str().unwrap()), s[1].as_str().unwrap(), "{}", s[0]);
    }
    for r in v["referenced"].as_array().unwrap() {
        assert_eq!(json!(games::referenced_assets(&r[0])), r[1]);
    }
    for b in v["boards"].as_array().unwrap() {
        assert_eq!(serde_json::to_value(scores::board_of(&b[0])).unwrap(), b[1], "{}", b[0]);
    }
    for w in v["worlds"].as_array().unwrap() {
        let uploaded: Vec<String> = serde_json::from_value(w[1].clone()).unwrap();
        assert_eq!(json!(worlds::check_world(&w[0], &uploaded, w[2].as_str())), w[3], "{}", w[0]);
    }
    for b in v["blocked"].as_array().unwrap() {
        assert_eq!(worlds::world_model_blocked(b[0].as_str().unwrap(), b[1].as_str()).map(Value::from).unwrap_or(Value::Null), b[2]);
    }
}

#[tokio::test]
async fn engine_checks_match_the_webassembly_build() {
    let v = v();
    let example = &v["example"];
    assert_eq!(canon(&engine::smoke_test(example, 10.0).await.unwrap()), canon(&v["smoke"]));
    assert_eq!(json!(engine::validate(&json!({ "start": "nowhere" }).as_object().map(|_| { let mut b = example.clone(); b["start"] = json!("nowhere"); b }).unwrap()).await.unwrap()), v["validateBroken"]);
    let checks = &v["checks"];
    let mut broken = example.clone();
    broken["start"] = json!("nowhere");
    for (name, game, uploaded) in [("example", example.clone(), vec![]), ("broken", broken, vec![]), ("notObject", json!(42), vec![])] {
        let got = serde_json::to_value(games::check_game(&game, &uploaded).await.unwrap()).unwrap();
        assert_eq!(canon(&got), canon(&checks[name]), "check {name}");
    }
    // TypeScript scripts: types stripped (the engine then checks the game as before).
    let mut with_ts = example.clone();
    with_ts["scripts"]["hud.ts"] = json!("export function draw(x: number): number { const y: string = 'a'; return x * 2 }\ninterface A { b: number }");
    let got = games::check_game(&with_ts, &[]).await.unwrap();
    assert_eq!(json!(got.problems), checks["withTs"]["problems"]);
    let (compiled, problems) = games::compile_scripts(&with_ts);
    assert!(problems.is_empty());
    let js = compiled["scripts"]["hud.js"].as_str().unwrap();
    assert!(js.contains("export function draw(x)") && !js.contains("interface") && compiled["scripts"]["hud.ts"].is_null(), "{js}");
    let mut bad = example.clone();
    bad["scripts"]["bad.ts"] = json!("export function (");
    let got = games::check_game(&bad, &[]).await.unwrap();
    assert_eq!(got.problems.len(), 1);
    assert!(got.problems[0].starts_with("script bad.ts: "), "{:?}", got.problems);
}

#[test]
fn play_tokens_and_payments_match() {
    let v = v();
    // The TypeScript core's token, byte for byte, rebuilt from its expiry (same SESSION_SECRET).
    let id = "g0123456789abcdef01";
    let theirs = v["playToken"].as_str().unwrap();
    use base64::Engine as _;
    let claim: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(theirs.split('.').next().unwrap()).unwrap()).unwrap();
    let issued = claim["exp"].as_i64().unwrap() - games::PLAY_TOKEN_SECONDS;
    assert_eq!(games::play_token_at(id, Some("0xABCdef0000000000000000000000000000000001"), false, issued), theirs);
    let mine = games::play_token_at(id, Some("0xABCdef0000000000000000000000000000000001"), false, chrono::Utc::now().timestamp());
    assert_eq!(games::verify_play_token(Some(&mine), id), Some((Some("0xabcdef0000000000000000000000000000000001".into()), false)));
    assert_eq!(games::verify_play_token(Some(&mine), "g0123456789abcdef02"), None);
    let tampered = format!("{}x", mine);
    assert_eq!(games::verify_play_token(Some(&tampered), id), None);
    let expired = games::play_token_at(id, None, true, chrono::Utc::now().timestamp() - 7 * 3600);
    assert_eq!(games::verify_play_token(Some(&expired), id), None);
}

#[tokio::test]
async fn payment_calls_match() {
    let v = v();
    support::shared_db().await;
    let g = game_row("g0123456789abcdef01", json!({ "kind": "paid", "priceUsd": 2.5, "tips": true }));
    for (got, want) in [
        (games::payment_call(&g, "tip", "usdt", Some(1.25)).await.unwrap(), &v["payTip"]),
        (games::payment_call(&g, "purchase", "usdc", None).await.unwrap(), &v["payBuy"]),
        (games::payment_call(&g, "tip", "bnb", Some(0.01)).await.unwrap(), &v["payBnb"]),
    ] {
        assert_eq!(got["chainId"], want["chainId"]);
        assert_eq!(got["amount"], want["amount"]);
        assert_eq!(got["token"].as_str().unwrap().to_lowercase(), want["token"].as_str().unwrap().to_lowercase());
        assert_eq!(got["calls"][0]["data"], want["calls"][0]["data"]);
        assert_eq!(got["calls"][0]["value"], want["calls"][0]["value"]);
        assert_eq!(got["calls"][0]["to"].as_str().unwrap().to_lowercase(), want["calls"][0]["to"].as_str().unwrap().to_lowercase());
    }
    assert!(games::payment_call(&g, "purchase", "bnb", None).await.is_err());
}

#[tokio::test]
async fn a_game_from_draft_to_leaderboard() {
    let v = v();
    let db = support::shared_db().await;
    let mut example = v["example"].clone();
    // A saved variable can't also be the score (the engine refuses it); "lives" is saved here.
    example["leaderboard"] = json!({ "var": "sparks" });
    example["save"] = json!(["lives"]);
    let creator = "0x1111111111111111111111111111111111111111";
    let (game, check) = games::create_game(games::NewGame { creator_account_id: creator, draft: &example, agent_id: None, hire_id: None, publish_as_agent: false }).await.unwrap();
    assert_eq!(game.slug, "lantern-run");
    assert_eq!(check.missing_assets, vec!["chime.wav"]);
    assert!(games::publish_game(&game).await.is_err(), "missing upload");
    // A WAV header is enough for the upload rules.
    let wav = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
    assert!(games::upload_asset(&game, "../evil.wav", &wav).await.is_err());
    assert!(games::upload_asset(&game, "chime.exe", &wav).await.is_err());
    games::upload_asset(&game, "chime.wav", &wav).await.unwrap();
    let (game, check) = games::save_draft(&game, &example).await.unwrap();
    assert!(check.ok, "{:?}", check.problems);
    let game = games::publish_game(&game).await.unwrap();
    assert_eq!((game.version, game.status.as_str()), (1, "published"));
    assert!(games::published_version(&game, 1).await.unwrap().is_some());
    let (again, _) = games::create_game(games::NewGame { creator_account_id: creator, draft: &example, agent_id: None, hire_id: None, publish_as_agent: false }).await.unwrap();
    assert!(again.slug.starts_with("lantern-run-") && again.slug.len() == "lantern-run-".len() + 4);
    assert_eq!(games::get_game(&game.slug).await.unwrap().unwrap().id, game.id);
    assert_eq!(games::access_for(&game, None).await.unwrap().allowed, true, "free");
    let paid = games::set_access(&game, &json!({ "kind": "paid", "priceUsd": 2.499, "tips": true }), None).await.unwrap();
    assert_eq!(paid.access, json!({ "kind": "paid", "priceUsd": 2.5, "tips": true }));
    assert_eq!(games::access_for(&paid, None).await.unwrap().need, Some("sign-in"));
    assert_eq!(games::access_for(&paid, Some("0x2222222222222222222222222222222222222222")).await.unwrap().need, Some("purchase"));
    assert!(games::access_for(&paid, Some(creator)).await.unwrap().allowed, "the creator plays their own game");
    assert!(games::set_access(&game, &json!({ "kind": "paid", "priceUsd": 0.1 }), None).await.is_err());

    // Saves and a replayed run: an idle run of 2 seconds ends "running", so it doesn't count.
    let player = "0x3333333333333333333333333333333333333333";
    let saved = scores::put_save(&game.id, player, &example, &json!({ "lives": 2, "other": 1 })).await.unwrap();
    assert_eq!(saved["data"], json!({ "lives": 2 }));
    let got = scores::get_save(&game.id, player, &example).await.unwrap();
    assert_eq!(got["token"], saved["token"]);
    let log = json!({ "frames": 120, "start": { "lives": 2 } });
    let err = scores::submit_run(scores::Run { game_id: &game.id, account_id: player, version: 1, game: &example, log: &log, save: Some("forged") }).await.unwrap_err();
    assert!(err.message.contains("save Orvyn didn't give out"));
    let err = scores::submit_run(scores::Run { game_id: &game.id, account_id: player, version: 1, game: &example, log: &json!({ "frames": 120 }), save: None }).await.unwrap_err();
    assert!(err.message.contains("hadn't ended"), "{}", err.message);
    assert_eq!(scores::leaderboard(&game.id, &example, None, Some(player)).await.unwrap().unwrap()["players"], 0);
    let _ = db;
}
