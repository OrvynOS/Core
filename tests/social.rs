//! Orvyn Social on a throwaway database and a mock BNB Chain (which answers who holds an Agent ID):
//! only Agent ID holders get in, handles are their Agent IDs, and posts, comments, reposts,
//! quotes, follows, feeds, profiles and images work; a person who sells their Agent ID is paused.
mod support;

use std::sync::{LazyLock, Mutex};

use orvyn_core::{social, Kind};
use serde_json::{json, Value};

const ME: &str = "0x00000000000000000000000000000000000000aa";
const OTHER: &str = "0x00000000000000000000000000000000000000bb";
const ALICE: &str = "user:0x00000000000000000000000000000000000000aa";
const ATLAS: &str = "agent:atlas-1";

/// Who the mock chain says holds any Agent ID (None: not minted).
static HOLDER: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(Some(ME.into())));

fn mock_chain() -> String {
    use axum::routing::post;
    support::serve_forever(axum::Router::new().route(
        "/",
        post(|axum::Json(body): axum::Json<Value>| async move {
            let answer = |req: &Value| match req["method"].as_str() {
                Some("eth_call") => match HOLDER.lock().unwrap().clone() {
                    Some(h) => json!({ "jsonrpc": "2.0", "id": req["id"], "result": format!("0x{:0>64}", h.trim_start_matches("0x")) }),
                    None => json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": 3, "message": "execution reverted" } }),
                },
                Some("eth_chainId") => json!({ "jsonrpc": "2.0", "id": req["id"], "result": "0x38" }),
                _ => json!({ "jsonrpc": "2.0", "id": req["id"], "result": "0x0" }),
            };
            axum::Json(match &body {
                Value::Array(reqs) => Value::Array(reqs.iter().map(answer).collect()),
                req => answer(req),
            })
        }),
    ))
}

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

#[tokio::test]
async fn social_network_for_agent_id_holders() {
    let rpc = mock_chain();
    support::test_env(&[("BSC_RPC_URL", &rpc)]);
    let db = support::shared_db().await;
    sqlx::query("insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status, agent_id_name, agent_id_verified_at) values ('atlas-1', 'atlas', 'Atlas', 'ATLAS', $1, 'x', 'x', 'x', 'x', '0x00000000000000000000000000000000000000a1', '{}', 'live', 'atlas', now())")
        .bind(ME)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status) values ('noid', 'noid', 'No ID', 'NO', $1, 'x', 'x', 'x', 'x', '0x00000000000000000000000000000000000000a2', '{}', 'live')")
        .bind(ME)
        .execute(db)
        .await
        .unwrap();

    // Not a member until they claim an Agent ID; agents need their own.
    let s = social::status(ALICE).await.unwrap();
    assert_eq!(s["member"], false);
    assert_eq!(s["canClaim"], true);
    assert_eq!(social::feed(ALICE, "latest", None).await.unwrap_err().kind, Kind::Forbidden);
    assert_eq!(social::member("agent:noid").await.unwrap_err().kind, Kind::Forbidden);
    assert_eq!(social::member("user:0xnotreal").await.unwrap_err().kind, Kind::Forbidden);

    // The Agent ID is the username.
    let p = social::claim(ALICE, " Alice.AI ").await.unwrap();
    assert_eq!(p.handle, "alice");
    let atlas = social::member(ATLAS).await.unwrap();
    assert_eq!((atlas.handle.as_str(), atlas.display_name.as_str(), atlas.kind.as_str()), ("atlas", "Atlas", "agent"));
    // A handle is one profile's: the owner can't take their agent's Agent ID.
    assert_eq!(social::claim(ALICE, "atlas.ai").await.unwrap_err().kind, Kind::Conflict);
    // Someone else can't claim what they don't hold.
    *HOLDER.lock().unwrap() = Some(ME.into());
    assert_eq!(social::claim(&format!("user:{OTHER}"), "bob.ai").await.unwrap_err().kind, Kind::Invalid);

    // Posts: text and images, within limits.
    assert_eq!(social::create_post(ALICE, "  ", &[], None, None, "app").await.unwrap_err().kind, Kind::Invalid);
    assert_eq!(social::create_post(ALICE, &"x".repeat(1001), &[], None, None, "app").await.unwrap_err().kind, Kind::Invalid);
    assert_eq!(social::upload_media(ALICE, b"not an image").await.unwrap_err().kind, Kind::Invalid);
    let img = social::upload_media(ALICE, PNG).await.unwrap();
    assert_eq!(img["mime"], "image/png");
    let img_id = img["id"].as_str().unwrap().to_string();
    assert_eq!(social::media(&img_id).await.unwrap().0, "image/png");
    // Someone else's image can't be attached.
    assert_eq!(social::create_post(ATLAS, "hi", &[img_id.clone()], None, None, "api").await.unwrap_err().kind, Kind::Invalid);
    let post = social::create_post(ALICE, "Hello, Orvyn Social!", &[img_id.clone()], None, None, "app").await.unwrap();
    assert_eq!(post["author"]["handle"], "alice");
    assert!(post["media"][0].as_str().unwrap().ends_with(&img_id));
    let pid: i64 = post["id"].as_str().unwrap().parse().unwrap();

    // Follows and feeds.
    assert_eq!(social::follow(ATLAS, "@alice.ai").await.unwrap()["followedByMe"], true);
    assert_eq!(social::follow(ALICE, "alice").await.unwrap_err().kind, Kind::Invalid);
    let f = social::feed(ATLAS, "following", None).await.unwrap();
    assert_eq!(f["posts"][0]["id"], post["id"]);
    assert_eq!(social::feed(ALICE, "following", None).await.unwrap()["posts"].as_array().unwrap().len(), 1, "your own posts are in your feed");
    let alice_view = social::profile_view(&social::profile_by_handle("alice").await.unwrap(), Some(ATLAS)).await.unwrap();
    assert_eq!((alice_view["followers"].as_i64(), alice_view["followedByMe"].as_bool(), alice_view["agentId"].as_str()), (Some(1), Some(true), Some("alice.ai")));
    assert_eq!(social::follow_list(ALICE, "alice", "followers", None).await.unwrap()["profiles"][0]["handle"], "atlas");

    // Comments: counted on the post, in its thread, not in feeds.
    let c = social::create_post(ATLAS, "Welcome!", &[], Some(pid), None, "autopilot").await.unwrap();
    assert_eq!(c["replyTo"]["handle"], "alice");
    let th = social::thread(ALICE, pid, None).await.unwrap();
    assert_eq!(th["post"]["replyCount"], 1);
    assert_eq!(th["replies"][0]["text"], "Welcome!");
    assert_eq!(social::feed(ALICE, "latest", None).await.unwrap()["posts"].as_array().unwrap().len(), 1);
    assert_eq!(social::profile_posts(ALICE, "atlas", "replies", None).await.unwrap()["posts"].as_array().unwrap().len(), 1);

    // Reposts: once each, shown as the original with who reposted it; undo works.
    social::repost(ATLAS, pid, "api").await.unwrap();
    let again = social::repost(ATLAS, pid, "api").await.unwrap();
    assert_eq!(again["repostCount"], 1);
    assert_eq!(again["repostedByMe"], true);
    let on_profile = social::profile_posts(ALICE, "atlas", "posts", None).await.unwrap();
    assert_eq!(on_profile["posts"][0]["repostedBy"]["handle"], "atlas");
    assert_eq!(on_profile["posts"][0]["id"], post["id"]);
    social::unrepost(ATLAS, pid).await.unwrap();
    assert_eq!(social::thread(ALICE, pid, None).await.unwrap()["post"]["repostCount"], 0);
    // Quotes need words, and show the quoted post.
    assert_eq!(social::create_post(ATLAS, "", &[], None, Some(pid), "api").await.unwrap_err().kind, Kind::Invalid);
    let q = social::create_post(ATLAS, "Worth a read", &[], None, Some(pid), "api").await.unwrap();
    assert_eq!(q["quote"]["id"], post["id"]);

    // Only the author deletes; comments stay under a placeholder.
    assert_eq!(social::delete_post(ATLAS, pid).await.unwrap_err().kind, Kind::NotFound);
    social::delete_post(ALICE, pid).await.unwrap();
    let gone = social::thread(ATLAS, pid, None).await.unwrap();
    assert_eq!(gone["post"]["deleted"], true);
    assert_eq!(gone["replies"].as_array().unwrap().len(), 1);

    // Profile edits and search.
    let edited = social::update_profile(ALICE, Some("Alice"), Some("Builds drones."), Some(&img_id)).await.unwrap();
    assert_eq!(edited.bio, "Builds drones.");
    assert_eq!(social::search(ATLAS, "ali").await.unwrap()["profiles"][0]["name"], "Alice");

    // Sold the Agent ID: paused at the daily re-check, back when they hold one again.
    sqlx::query("update social_profiles set verified_at = now() - interval '2 days' where subject = $1").bind(ALICE).execute(db).await.unwrap();
    *HOLDER.lock().unwrap() = Some(OTHER.into());
    assert_eq!(social::member(ALICE).await.unwrap_err().kind, Kind::Forbidden);
    assert!(social::feed(ATLAS, "latest", None).await.unwrap()["posts"].as_array().unwrap().iter().all(|p| p["author"]["handle"] != "alice"));
    *HOLDER.lock().unwrap() = Some(ME.into());
    social::claim(ALICE, "alice.ai").await.unwrap();
    assert!(social::member(ALICE).await.is_ok());
}
