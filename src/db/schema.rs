//! Row types for Orvyn's tables (the schema is the SQL in migrations/). Serialized in camelCase, as
//! the TypeScript core's rows were, so API responses keep their shape.
//!
//! `jsonb` columns are `serde_json::Value`; typed views of them live with the code that uses them.
//! `numeric` columns (wallet_spend.amount) are read as text in their queries.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;

pub type Time = DateTime<Utc>;

macro_rules! row {
    ($(#[$m:meta])* $name:ident { $($(#[$fm:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, FromRow, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $name { $($(#[$fm])* pub $field: $ty),* }
    };
}

row!(
    /// An agent: identity, settings, billing and hosting, its AI key (encrypted), wallet and token.
    Agent {
        id: String,
        handle: Option<String>,
        name: String,
        symbol: String,
        description: String,
        icon: Option<String>,
        links: Option<Value>,
        owner_account_id: String,
        persona: String,
        strategy: String,
        autopilot_enabled: bool,
        autopilot_interval_min: i32,
        companion: bool,
        billing_mode: String,
        registered: bool,
        paid_until: Option<Time>,
        subscription_active: bool,
        hosting_status: String,
        tier: String,
        hardware: Option<Value>,
        environment_hash: Option<String>,
        held_reason: Option<String>,
        render_service_id: Option<String>,
        host_url: Option<String>,
        host_internal: Option<String>,
        pending_treasury_months: i32,
        rate_limit_per_minute: i32,
        agent_id_name: Option<String>,
        agent_id_verified_at: Option<Time>,
        erc8004_agent_id: Option<String>,
        erc8004_error: Option<String>,
        billing_details: String,
        tax: Option<Value>,
        tax_allocated_wei: String,
        min_holding_tokens: f64,
        chat_messages_per_hour: i32,
        gas_reserve_bnb: f64,
        ai_provider: String,
        ai_model: String,
        ai_key_encrypted: String,
        ai_key_hint: String,
        evm_address: String,
        evm_wallet: Value,
        status: String,
        status_detail: Option<String>,
        launchpad: String,
        launch_request_id: Option<String>,
        token_address: Option<String>,
        launch_tx: Option<String>,
        graduated: bool,
        created_at: Time,
    }
);

row!(Trade {
    id: i32,
    agent_id: String,
    plugin_id: String,
    venue: String,
    symbol: String,
    side: String,
    size: f64,
    size_unit: String,
    leverage: Option<i32>,
    price: Option<f64>,
    rationale: String,
    result: Option<Value>,
    ok: bool,
    created_at: Time,
});

row!(
    /// A call: a direction on a token or coin over a horizon, graded against the real price.
    AgentCall {
        id: i32,
        agent_id: String,
        subject_kind: String,
        subject_ref: String,
        label: String,
        direction: String,
        confidence: f64,
        horizon_hours: i32,
        thesis: String,
        evidence: Option<Value>,
        entry_price: f64,
        due_at: Time,
        graded_at: Option<Time>,
        exit_price: Option<f64>,
        move_pct: Option<f64>,
        outcome: Option<String>,
        created_at: Time,
    }
);

row!(FeeClaim { id: i32, agent_id: String, kind: String, amount: Option<String>, tx: Option<String>, created_at: Time });

row!(ChatMessage { id: i32, agent_id: String, account_id: String, role: String, content: String, created_at: Time });

row!(AgentLog { id: i32, agent_id: String, kind: String, message: String, created_at: Time });

row!(
    /// One subscription payment in the registry's payment log, for invoices and receipts.
    Invoice {
        id: i32,
        agent_id: String,
        payment_index: i32,
        payer: String,
        token: String,
        symbol: String,
        amount: String,
        decimals: i32,
        months: i32,
        period_start: Time,
        period_end: Time,
        paid_at: Time,
        block_number: String,
        tx_hash: Option<String>,
        created_at: Time,
    }
);

row!(BinancePayOrder {
    id: String,
    agent_id: String,
    months: i32,
    amount_usd: String,
    status: String,
    prepay_id: Option<String>,
    checkout_url: Option<String>,
    created_by: Option<String>,
    tx_hash: Option<String>,
    error: Option<String>,
    expires_at: Time,
    paid_at: Option<Time>,
    credited_at: Option<Time>,
    created_at: Time,
});

row!(TriggerOrder {
    id: i32,
    agent_id: String,
    plugin_id: String,
    market: String,
    kind: String,
    trigger_price: f64,
    price_unit: String,
    sell_percent: i32,
    source: String,
    status: String,
    attempts: i32,
    note: Option<String>,
    result: Option<Value>,
    created_at: Time,
    triggered_at: Option<Time>,
});

row!(Withdrawal {
    id: i32,
    agent_id: String,
    requested_by: String,
    asset: String,
    symbol: String,
    amount: String,
    destination: String,
    tx: Option<String>,
    status: String,
    error: Option<String>,
    created_at: Time,
});

row!(
    /// A Bliss plugin installed on an agent: the creator's settings and the plugin's own state.
    AgentPlugin {
        agent_id: String,
        plugin_id: String,
        config: Value,
        state: Value,
        enabled: bool,
        #[sqlx(rename = "created_at")]
        installed_at: Time,
    }
);

row!(ExternalPlugin {
    slug: String,
    submitted_by: String,
    live: Option<Value>,
    submission: Option<Value>,
    review_note: Option<String>,
    reviewed_by: Option<String>,
    reviewed_at: Option<Time>,
    suspended: bool,
    created_at: Time,
    updated_at: Time,
});

row!(KnowledgeDoc { id: i32, agent_id: String, title: String, content: String, created_at: Time });

row!(Job { agent_id: String, task: String, next_run_at: Time, locked_until: Option<Time>, last_error: Option<String> });

row!(
    /// A message in an agent's mailbox at <handle>@orvyn.cash.
    MailMessage {
        id: i32,
        agent_id: String,
        direction: String,
        status: String,
        author: Option<String>,
        message_id: String,
        in_reply_to: Option<String>,
        references: Value,
        thread_id: String,
        from_address: String,
        from_name: String,
        to_addresses: Value,
        cc_addresses: Value,
        subject: String,
        text: String,
        attachments: Value,
        auth: Option<Value>,
        auto_submitted: bool,
        read: bool,
        handled: bool,
        delivery: Value,
        attempts: i32,
        next_attempt_at: Option<Time>,
        locked_until: Option<Time>,
        last_error: Option<String>,
        sent_at: Option<Time>,
        created_at: Time,
    }
);

row!(MailSettings {
    agent_id: String,
    enabled: bool,
    display_name: Option<String>,
    blocked: Value,
    forward_to_owner: bool,
    updated_at: Time,
});


row!(
    /// A marketplace job, paid into the escrow contract. `id` is the job id on chain.
    Hire {
        id: String,
        agent_id: String,
        client_account_id: String,
        client_email: Option<String>,
        client_country: Option<String>,
        offer_title: String,
        brief: String,
        token: String,
        amount: String,
        fee_bps: i32,
        status: String,
        deadline_at: Time,
        funded_at: Option<Time>,
        delivered_at: Option<Time>,
        review_ends_at: Option<Time>,
        settled_at: Option<Time>,
        result: Option<String>,
        result_hash: Option<String>,
        needs_agent: bool,
        agent_attempts: i32,
        chain_action: Option<String>,
        chain_error: Option<String>,
        paid_to_agent: Option<String>,
        refunded: Option<String>,
        fee: Option<String>,
        rating: Option<i32>,
        review: Option<String>,
        email_thread_id: Option<String>,
        created_at: Time,
        updated_at: Time,
    }
);

row!(HireMessage { id: i32, hire_id: String, author: String, text: String, via_email: bool, created_at: Time });

row!(StudioRender {
    id: String,
    agent_id: String,
    hire_id: Option<String>,
    renderer: String,
    worker_url: String,
    ok: bool,
    error: Option<String>,
    duration_ms: i32,
    created_at: Time,
});

row!(JobFile {
    id: String,
    render_id: String,
    agent_id: String,
    hire_id: Option<String>,
    name: String,
    mime: String,
    bytes: i32,
    sha256: String,
    worker_url: String,
    check_ok: bool,
    check_notes: Value,
    delivered: bool,
    created_at: Time,
});

row!(
    /// A person who signs in. `account_id` is always an EVM address (lowercase).
    User {
        account_id: String,
        sign_in_method: String,
        privy_user_id: Option<String>,
        email: Option<String>,
        social_provider: Option<String>,
        social_username: Option<String>,
        created_at: Time,
        last_sign_in_at: Time,
    }
);

row!(PushDevice { token: String, account_id: String, platform: String, created_at: Time, last_seen_at: Time });

row!(WalletLogEntry { id: i32, agent_id: String, scheme: String, summary: Option<String>, ok: bool, error: Option<String>, created_at: Time });

row!(ShieldedKey {
    agent_id: String,
    public_key: String,
    encryption_public: String,
    sealed: String,
    registered_tx: Option<String>,
    created_at: Time,
});

row!(OauthClient {
    id: String,
    name: String,
    owner_account_id: String,
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    kind: String,
    secret_hash: Option<String>,
    redirect_uris: Value,
    scopes: Value,
    homepage: Option<String>,
    logo_url: Option<String>,
    first_party: bool,
    status: String,
    created_at: Time,
});

row!(OauthGrant {
    id: i32,
    client_id: String,
    account_id: String,
    subject: String,
    scope: String,
    created_at: Time,
    revoked_at: Option<Time>,
});

row!(OauthCode {
    code_hash: String,
    client_id: String,
    grant_id: i32,
    subject: String,
    scope: String,
    redirect_uri: String,
    code_challenge: String,
    nonce: Option<String>,
    expires_at: Time,
    used_at: Option<Time>,
});

row!(OauthRefreshToken {
    id: i32,
    token_hash: String,
    family_id: String,
    client_id: String,
    grant_id: i32,
    subject: String,
    scope: String,
    expires_at: Time,
    used_at: Option<Time>,
    revoked_at: Option<Time>,
    created_at: Time,
});

row!(OauthDeviceCode {
    device_code_hash: String,
    user_code: String,
    client_id: String,
    scope: String,
    status: String,
    grant_id: Option<i32>,
    subject: Option<String>,
    expires_at: Time,
    last_polled_at: Option<Time>,
    created_at: Time,
});

row!(AgentCard {
    agent_id: String,
    owner_account_id: String,
    card_id: String,
    environment: String,
    chain_id: String,
    asset: String,
    enabled: bool,
    per_purchase_limit_usd: f64,
    monthly_limit_usd: f64,
    created_at: Time,
});

row!(
    /// A person's Vyn settings: their AI key (encrypted), model, payment limits, persona.
    VynSettings {
        account_id: String,
        ai_provider: String,
        ai_model: String,
        ai_key_encrypted: String,
        ai_key_hint: String,
        auto_approve_usd: f64,
        persona: Value,
        timezone: String,
        check_in_time: Option<String>,
        updated_at: Time,
    }
);

row!(VynMemory { id: i32, account_id: String, text: String, kind: String, created_at: Time });

row!(VynRoutine {
    id: String,
    account_id: String,
    title: String,
    instruction: String,
    schedule: Value,
    kind: String,
    next_run_at: Option<Time>,
    last_run_at: Option<Time>,
    active: bool,
    created_at: Time,
});

row!(VynNotice { id: i32, account_id: String, thread_id: String, title: String, text: String, created_at: Time });

row!(VynChannel {
    id: i32,
    account_id: String,
    kind: String,
    external_id: String,
    thread_id: String,
    label: Option<String>,
    created_at: Time,
});

row!(VynLinkCode { code: String, account_id: String, kind: String, expires_at: Time });

row!(VynThread { id: String, account_id: String, title: String, busy_until: Option<Time>, created_at: Time, updated_at: Time });

row!(VynMessage { id: i32, thread_id: String, message: Value, device: Option<String>, created_at: Time });

row!(ExternalAgentKey { key_hash: String, agent_id: String, created_at: Time, revoked_at: Option<Time> });

row!(RngNode {
    agent_id: String,
    public_key: String,
    secret_encrypted: Option<String>,
    external: bool,
    active: bool,
    chain_index: Option<i32>,
    #[sqlx(rename = "created_at")]
    joined_at: Time,
});

row!(RngPartial { round: i32, agent_id: String, signature: String, created_at: Time });

row!(RngRound {
    round: i32,
    output: String,
    aggregate_signature: String,
    participants: Value,
    drand_round: i32,
    drand_signature: String,
    commit_tx: Option<String>,
    fulfill_tx: Option<String>,
    created_at: Time,
});

row!(PriceObservation { feed: String, round: i32, agent_id: String, price: String, signature: String, created_at: Time });

row!(PriceReport {
    feed: String,
    round: i32,
    price: String,
    observations: i32,
    participants: Option<Value>,
    aggregate_signature: Option<String>,
    submit_tx: Option<String>,
    created_at: Time,
});

row!(PriceReportSignature { feed: String, round: i32, agent_id: String, signature: String, created_at: Time });

row!(
    /// An Orvyn Engine game (or dream world). `draft` is the working copy; published versions are
    /// in game_versions.
    Game {
        id: String,
        slug: String,
        title: String,
        description: String,
        creator_account_id: String,
        agent_id: Option<String>,
        publish_as_agent: bool,
        hire_id: Option<String>,
        status: String,
        access: Value,
        draft: Value,
        problems: Value,
        version: i32,
        plays: i32,
        #[serde(skip)]
        thumbnail: Option<Vec<u8>>,
        thumbnail_version: Option<i32>,
        published_at: Option<Time>,
        created_at: Time,
        updated_at: Time,
    }
);

row!(GameVersion { game_id: String, version: i32, game: Value, sha256: String, created_at: Time });

row!(GameAsset {
    game_id: String,
    name: String,
    mime: String,
    #[serde(skip)]
    bytes: Vec<u8>,
    size: i32,
    sha256: String,
    created_at: Time,
});

row!(GamePayment {
    tx_hash: String,
    game_id: String,
    account_id: String,
    kind: String,
    token: String,
    amount: String,
    usd: Option<f64>,
    created_at: Time,
});

row!(GameScore {
    game_id: String,
    account_id: String,
    value: f64,
    version: i32,
    frames: i32,
    runs: i32,
    created_at: Time,
    updated_at: Time,
});

row!(WorldSession {
    id: String,
    game_id: String,
    account_id: String,
    model: String,
    gpu: bool,
    country: Option<String>,
    preview: bool,
    allowed_seconds: i32,
    seconds: Option<f64>,
    ended_at: Option<Time>,
    expires_at: Time,
    created_at: Time,
});

row!(GameSave { game_id: String, account_id: String, data: Value, updated_at: Time });

row!(BinanceConnection {
    id: i32,
    account_id: String,
    agent_id: Option<String>,
    access_token_encrypted: String,
    refresh_token_encrypted: Option<String>,
    scope: Option<String>,
    expires_at: Option<Time>,
    tools: Value,
    created_at: Time,
    updated_at: Time,
});

row!(BinanceOauthState { state: String, account_id: String, agent_id: Option<String>, verifier_encrypted: String, return_to: Option<String>, expires_at: Time });

row!(BinanceAgentOrder { id: i32, agent_id: String, tool: String, symbol: Option<String>, notional_usd: f64, created_at: Time });

// ---- Orvyn inference: credits, API keys, chat ----

row!(CreditEntry {
    id: i32,
    account_id: String,
    /// deposit | usage | adjust
    kind: String,
    /// Signed: deposits add, usage takes away.
    micro_usd: i64,
    tx_hash: Option<String>,
    token: Option<String>,
    amount: Option<String>,
    model: Option<String>,
    /// chat | api | agent
    source: Option<String>,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    created_at: Time,
});

row!(InferenceKey { id: String, account_id: String, name: String, key_hash: String, hint: String, created_at: Time, last_used_at: Option<Time>, revoked_at: Option<Time> });

row!(ChatThread { id: String, account_id: String, title: String, model: String, created_at: Time, updated_at: Time });

row!(ChatTurnMessage { id: i32, thread_id: String, role: String, content: String, model: Option<String>, micro_usd: i64, input_tokens: Option<i32>, output_tokens: Option<i32>, created_at: Time });
