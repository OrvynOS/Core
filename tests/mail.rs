//! Mail, the marketplace and Studio against the TypeScript core (vectors/mail.json), and a mail
//! round trip on a throwaway database: queue, claim, retry, deliver, receive, thread, bounce, and
//! a client's emailed reply landing on their job.
mod support;

use std::collections::BTreeMap;

use orvyn_core::db::{Agent, Hire, JobFile};
use orvyn_core::{hires, mail, studio};
use serde_json::{json, Value};

const ENV: &[(&str, &str)] = &[
    ("ORVYN_MAIL_API_KEY", "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm"),
    ("ORVYN_MAIL_DOMAIN", "orvyn.cash"),
    ("ORVYN_JOBS_CONTRACT", "0x5555555555555555555555555555555555555555"),
];

fn v() -> Value {
    support::test_env(ENV);
    serde_json::from_str(include_str!("vectors/mail.json")).unwrap()
}

#[test]
fn addresses_rules_and_bounces_match() {
    let v = v();
    for e in v["emails"].as_array().unwrap() {
        let input = e[0].as_str().unwrap();
        assert_eq!(mail::normalize_email(input).map(Value::from).unwrap_or(Value::Null), e[1], "normalize {input:?}");
        let split = mail::split_address(input).map(|p| json!({ "local": p.local, "tag": p.tag, "domain": p.domain })).unwrap_or(Value::Null);
        assert_eq!(split, e[2], "split {input:?}");
    }
    for l in v["lists"].as_array().unwrap() {
        let list: Vec<String> = serde_json::from_value(l[1].clone()).unwrap();
        assert_eq!(Value::Bool(mail::matches_list(l[0].as_str().unwrap(), &list)), l[2], "{l}");
    }
    for a in v["autos"].as_array().unwrap() {
        let m = &a[0];
        let got = mail::is_automated(m["autoSubmitted"].as_str(), m["precedence"].as_str(), m["listId"].as_str(), m["mailFrom"].as_str().unwrap(), m["fromAddress"].as_str().unwrap());
        assert_eq!(Value::Bool(got), a[1], "{m}");
    }
    for s in v["spam"].as_array().unwrap() {
        let auth: mail::Auth = serde_json::from_value(s[0].clone()).unwrap();
        assert_eq!(Value::Bool(mail::is_spam(&auth)), s[1], "{}", s[0]);
    }
    assert_eq!(mail::bounce_address(42), v["bounce"].as_str().unwrap());
    for r in v["replySubject"].as_array().unwrap() {
        assert_eq!(mail::reply_subject(r[0].as_str().unwrap()), r[1].as_str().unwrap());
    }
    for q in v["quoted"].as_array().unwrap() {
        assert_eq!(hires::strip_quoted(q[0].as_str().unwrap()), q[1].as_str().unwrap(), "{}", q[0]);
    }
}

#[test]
fn escrow_calls_offers_and_studio_rules_match() {
    let v = v();
    let now = chrono::Utc::now();
    let hire = |id: &str, token: &str, amount: &str| Hire {
        id: id.into(),
        agent_id: "agent-1".into(),
        client_account_id: "0xc".into(),
        client_email: None,
        client_country: None,
        offer_title: "T".into(),
        brief: "B".into(),
        token: token.into(),
        amount: amount.into(),
        fee_bps: 500,
        status: "awaiting_payment".into(),
        deadline_at: chrono::DateTime::from_timestamp_millis(1795000000000).unwrap(),
        funded_at: None,
        delivered_at: None,
        review_ends_at: None,
        settled_at: None,
        result: None,
        result_hash: None,
        needs_agent: false,
        agent_attempts: 0,
        chain_action: None,
        chain_error: None,
        paid_to_agent: None,
        refunded: None,
        fee: None,
        rating: None,
        review: None,
        email_thread_id: None,
        created_at: now,
        updated_at: now,
    };
    let payee = "0x6666666666666666666666666666666666666666";
    let check = |got: orvyn_core::billing::Calls, want: &Value| {
        assert_eq!(got.chain_id, want["chainId"].as_u64().unwrap());
        let want = want["calls"].as_array().unwrap();
        assert_eq!(got.calls.len(), want.len());
        for (g, w) in got.calls.iter().zip(want) {
            assert_eq!(g.to.to_lowercase(), w["to"].as_str().unwrap().to_lowercase());
            assert_eq!(g.data, w["data"].as_str().unwrap());
            assert_eq!(g.value, w["value"].as_str().unwrap());
        }
    };
    check(hires::payment_tx(&hire("habc123", "0x55d398326f99059ff775485246999027b3197955", "25000000000000000000"), "agent-1", payee).unwrap(), &v["paymentTx"]);
    check(hires::payment_tx(&hire("hbnb", "native", "40000000000000000"), "agent-1", payee).unwrap(), &v["paymentTxBnb"]);
    for (action, want) in ["approve", "dispute", "reclaim"].iter().zip(v["clientCalls"].as_array().unwrap()) {
        check(hires::client_call("habc123", action, "not what I asked for").unwrap(), want);
    }
    for t in v["toUsd"].as_array().unwrap() {
        assert_eq!(hires::to_usd(t[0].as_str().unwrap()), t[1].as_f64().unwrap());
    }
    for o in v["offers"].as_array().unwrap() {
        let got = serde_json::to_value(hires::offer_of(&o[0])).unwrap();
        let mut want = o[1].clone();
        want.as_object_mut().unwrap().remove("instructions");
        assert_eq!(support_canon(&got), support_canon(&want));
    }
    assert_eq!(hires::result_hash("the result ✓"), v["resultHash"].as_str().unwrap());
    let files: Vec<JobFile> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| JobFile {
            id: "f".into(),
            render_id: "r".into(),
            agent_id: "a".into(),
            hire_id: None,
            name: f["name"].as_str().unwrap().into(),
            mime: "x".into(),
            bytes: f["bytes"].as_i64().unwrap() as i32,
            sha256: f["sha256"].as_str().unwrap().into(),
            worker_url: String::new(),
            check_ok: true,
            check_notes: json!([]),
            delivered: false,
            created_at: now,
        })
        .collect();
    assert_eq!(studio::files_manifest(&files), v["manifest"].as_str().unwrap());
    for m in v["models"].as_array().unwrap() {
        let got = studio::model_blocked(m[0].as_str().unwrap(), m[1].as_str().unwrap(), m[2].as_str());
        assert_eq!(got.map(Value::from).unwrap_or(Value::Null), m[3], "{m}");
    }
    for r in v["renderBlocked"].as_array().unwrap() {
        let got = studio::render_blocked(orvyn_core::tiers::Tier::parse(r[0].as_str().unwrap()), r[1].as_str().unwrap(), r[2].as_str());
        assert_eq!(got.map(Value::from).unwrap_or(Value::Null), r[3], "{r}");
    }
}

/// Numbers compared as numbers (12.5 vs "12.5" never mixes; 24 vs 24.0 does).
fn support_canon(v: &Value) -> String {
    orvyn_core::environment::canonical_json(v)
}

#[tokio::test]
async fn a_mail_round_trip() {
    support::test_env(ENV);
    let db = support::shared_db().await;
    sqlx::query(
        "insert into agents (id, handle, name, symbol, owner_account_id, ai_provider, ai_model, ai_key_encrypted, ai_key_hint, evm_address, evm_wallet, status)
         values ('a1', 'nova', 'Nova', 'NOVA', '0xowner', 'x', 'x', 'x', 'x', '0x2', '{}', 'live')",
    )
    .execute(db)
    .await
    .unwrap();
    let agent: Agent = sqlx::query_as("select * from agents where id = 'a1'").fetch_one(db).await.unwrap();

    // Recipients.
    assert!(matches!(mail::resolve_recipient("Nova+tag@orvyn.cash").await.unwrap(), mail::Recipient::Agent { ref agent_id, .. } if agent_id == "a1"));
    assert_eq!(mail::resolve_recipient("nobody@orvyn.cash").await.unwrap(), mail::Recipient::Unknown);
    assert_eq!(mail::resolve_recipient("nova@elsewhere.com").await.unwrap(), mail::Recipient::Unknown);
    assert_eq!(mail::resolve_recipient("abuse@orvyn.cash").await.unwrap(), mail::Recipient::System { local: "abuse".into() });

    // Outbound: validation, queue, claim, a temporary failure, then delivery.
    let out = |to: Vec<&str>, text: &str| mail::Outgoing { agent: &agent, author: "agent", to: to.into_iter().map(String::from).collect(), cc: vec![], subject: "Hello\r\nthere".into(), text: text.into(), reply_to: None, auto_submitted: None };
    assert!(mail::queue_message(out(vec!["nova@orvyn.cash"], "hi")).await.is_err(), "can't email itself");
    assert!(mail::queue_message(out(vec!["not-an-address"], "hi")).await.is_err());
    assert!(mail::queue_message(out(vec!["bob@example.com"], "   ")).await.is_err(), "empty");
    let sent = mail::queue_message(out(vec!["Bob@Example.com", "bob@example.com", "carol@example.com"], "Hi Bob")).await.unwrap();
    assert_eq!(sent.subject, "Hello there");
    assert_eq!(sent.to_addresses, json!(["bob@example.com", "carol@example.com"]));
    assert!(sent.auto_submitted);
    let jobs = mail::claim_outbound(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].pending, vec!["bob@example.com", "carol@example.com"]);
    assert!(jobs[0].envelope_from.starts_with(&format!("bounces+{}-", sent.id)));
    assert!(mail::claim_outbound(10).await.unwrap().is_empty(), "leased");
    let mut results = BTreeMap::new();
    results.insert("bob@example.com".to_string(), mail::DeliveryResult { status: "sent".into(), error: None });
    results.insert("carol@example.com".to_string(), mail::DeliveryResult { status: "temp".into(), error: Some("451 try later".into()) });
    assert_eq!(mail::report_delivery(sent.id as i64, &results).await.unwrap(), Some("retry"));
    let row = mail::get_message("a1", sent.id as i64).await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.attempts, 1);
    assert_eq!(row.delivery["bob@example.com"]["status"], "sent");
    let mut results = BTreeMap::new();
    results.insert("carol@example.com".to_string(), mail::DeliveryResult { status: "perm".into(), error: Some("550 no such user".into()) });
    assert_eq!(mail::report_delivery(sent.id as i64, &results).await.unwrap(), Some("sent"));
    assert_eq!(mail::sent_last_24h("a1", Some("agent")).await.unwrap(), 1);
    assert_eq!(mail::replies_to("a1", "bob@example.com").await.unwrap(), 1);

    // A bounce for it comes back to its VERP address.
    let mail::Recipient::Bounce { message_id } = mail::resolve_recipient(&jobs[0].envelope_from).await.unwrap() else { panic!("not a bounce address") };
    assert_eq!(message_id, sent.id as i64);
    assert_eq!(mail::resolve_recipient(&format!("bounces+{}-000000000000@orvyn.cash", sent.id)).await.unwrap(), mail::Recipient::Unknown, "forged tag");
    assert!(mail::mark_bounced(message_id, "550 user unknown").await.unwrap());

    // Inbound: Bob's reply joins the thread; spam and automated mail never reach the agent.
    let inbound = |from: &str, subject: &str, in_reply_to: Option<String>| mail::InboundMessage {
        recipients: vec!["nova@orvyn.cash".into()],
        mail_from: from.into(),
        auth: mail::Auth { spf: "pass".into(), dkim: "pass".into(), dmarc: "pass".into(), dmarc_policy: None },
        message_id: Some(format!("<{subject}@example.com>")),
        in_reply_to,
        from: Some(mail::NameAddress { address: from.into(), name: "Bob".into() }),
        to: vec!["nova@orvyn.cash".into()],
        subject: subject.into(),
        text: "Thanks!\n\nOn Mon, Nova wrote:\n> Hi Bob".into(),
        ..Default::default()
    };
    assert_eq!(mail::receive_message(&inbound("bob@example.com", "r1", Some(sent.message_id.clone()))).await.unwrap(), 1);
    let mut spam = inbound("eve@example.com", "s1", None);
    spam.auth = mail::Auth { spf: "fail".into(), dkim: "fail".into(), dmarc: "fail".into(), dmarc_policy: Some("reject".into()) };
    mail::receive_message(&spam).await.unwrap();
    let mut auto = inbound("bob@example.com", "a1", None);
    auto.auto_submitted = Some("auto-replied".into());
    mail::receive_message(&auto).await.unwrap();
    let todo = mail::unhandled("a1", 10).await.unwrap();
    assert_eq!(todo.len(), 1);
    assert_eq!(todo[0].thread_id, sent.thread_id);
    assert_eq!(mail::list_messages("a1", "spam", None, None).await.unwrap().len(), 1);
    assert_eq!(mail::list_messages("a1", "inbox", None, None).await.unwrap().len(), 2);
    assert_eq!(mail::thread_messages("a1", &sent.thread_id, 20).await.unwrap().len(), 2);
    assert_eq!(mail::unread_count("a1").await.unwrap(), 2);
    mail::mark_handled(&[todo[0].id]).await.unwrap();
    assert!(mail::unhandled("a1", 10).await.unwrap().is_empty());

    // Settings: blocked senders land in spam.
    let patch: mail::SettingsPatch = serde_json::from_value(json!({ "blocked": ["Example.com", "example.com"], "displayName": "Nova <bot>" })).unwrap();
    let s = mail::update_mail_settings("a1", patch).await.unwrap();
    assert_eq!(s.blocked, vec!["example.com"]);
    assert_eq!(s.display_name.as_deref(), Some("Nova  bot"));
    let bad: mail::SettingsPatch = serde_json::from_value(json!({ "blocked": ["not a domain"] })).unwrap();
    assert!(mail::update_mail_settings("a1", bad).await.is_err());
    mail::receive_message(&inbound("bob@example.com", "r2", None)).await.unwrap();
    assert_eq!(mail::list_messages("a1", "spam", None, None).await.unwrap().len(), 2);

    // A client's reply in a job's email thread goes to the job.
    mail::update_mail_settings("a1", serde_json::from_value(json!({ "blocked": [] })).unwrap()).await.unwrap();
    sqlx::query(
        "insert into hires (id, agent_id, client_account_id, client_email, offer_title, brief, token, amount, fee_bps, status, deadline_at, email_thread_id)
         values ('h1', 'a1', '0xclient', 'bob@example.com', 'Logo', 'Make a logo', 'native', '1', 500, 'funded', now() + interval '1 day', $1)",
    )
    .bind(&sent.thread_id)
    .execute(db)
    .await
    .unwrap();
    mail::receive_message(&inbound("bob@example.com", "r3", Some(sent.message_id.clone()))).await.unwrap();
    assert!(mail::unhandled("a1", 10).await.unwrap().is_empty(), "consumed by the job");
    let msgs = hires::hire_messages_of("h1").await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].author, "client");
    assert_eq!(msgs[0].text, "Thanks!");
    assert!(msgs[0].via_email);
    let hire = hires::get_hire("h1").await.unwrap().unwrap();
    assert!(hire.needs_agent, "a client message wakes the agent");
    assert_eq!(hires::hire_view(&hire, &msgs, "owner")["clientEmail"], "(set)");
}
