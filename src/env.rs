//! Settings, read once from the environment. Same names, defaults and checks as the TypeScript
//! core's env.ts; empty values (e.g. Render Blueprint placeholders not yet filled in) count as unset.

use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostingMode {
    Shared,
    Render,
    SelfHosted,
}

#[derive(Clone, Debug)]
pub struct Env {
    // Postgres
    pub database_url: String,
    pub database_pool_size: u32,

    // Privy
    pub privy_app_id: Option<String>,
    pub privy_app_secret: Option<String>,
    pub privy_authorization_key: Option<String>,
    pub privy_authorization_key_id: Option<String>,
    pub privy_allow_app_owned_wallets: bool,
    pub privy_verification_key: Option<String>,
    pub privy_agent_policy_id: Option<String>,
    pub privy_aster_policy_id: Option<String>,

    // Aster, Opinion
    pub aster_api_url: String,
    pub opinion_api_url: String,
    pub opinion_api_key: Option<String>,

    pub ai_key_encryption_secret: String,
    pub bsc_rpc_url: Option<String>,
    pub session_secret: String,

    // Orvyn API
    pub orvyn_api_url: String,
    pub orvyn_internal_api_key: Option<String>,
    pub orvyn_api_keys: String,

    // Token launches
    pub fourmeme_api_url: String,
    pub fourmeme_token_manager: String,
    pub fourmeme_helper: String,
    pub flap_portal: Option<String>,
    pub tax_allocate_min_bnb: f64,

    // Billing
    pub orvyn_registry_contract: Option<String>,
    pub orvyn_operator_private_key: Option<String>,
    pub orvyn_rng_contract: Option<String>,
    pub orvyn_rng_relayer_key: Option<String>,
    pub binance_pay_api_key: Option<String>,
    pub binance_pay_secret: Option<String>,
    pub binance_pay_api_url: String,
    pub binance_pay_webhook_url: Option<String>,
    pub orvyn_binance_pay_creditor_key: Option<String>,
    pub billing_renew_days: f64,
    pub billing_grace_hours: f64,

    // Identity
    pub orvyn_attestation_key: Option<String>,
    pub erc8004_enabled: bool,
    pub erc8004_chain: String,
    pub erc8004_registry: String,
    pub orvyn_erc8004_sponsor_key: Option<String>,

    // OrvynPay
    pub orvyn_pay_contract: Option<String>,
    pub orvyn_shielded_pool: Option<String>,

    // Third-party plugins
    pub orvyn_admin_accounts: String,
    pub bliss_allow_private_urls: bool,

    // Orvyn inference (chat and /v1/chat/completions, paid from credits)
    /// The upstream multi-model key. Server-side only: never sent to the app or named in it.
    pub openrouter_api_key: Option<String>,
    /// Where credit deposits (BNB, USDT, USDC on BNB Chain) are sent.
    pub orvyn_credits_address: Option<String>,
    /// What people pay over the upstream cost (1.2 = 20% more).
    pub inference_markup: f64,

    // Invoices
    pub orvyn_billing_entity: String,
    pub orvyn_billing_address: Option<String>,
    pub orvyn_billing_email: Option<String>,
    pub orvyn_billing_tax_id: Option<String>,

    // Mail
    pub orvyn_mail_domain: String,
    pub orvyn_mail_api_key: Option<String>,
    pub orvyn_mail_daily_send_cap: u32,
    pub orvyn_mail_daily_receive_cap: u32,

    // Marketplace
    pub orvyn_jobs_contract: Option<String>,
    pub orvyn_jobs_operator_private_key: Option<String>,

    // Hosting
    pub hosting_mode: HostingMode,
    pub render_api_key: Option<String>,
    pub render_api_url: String,
    pub render_owner_id: Option<String>,
    pub render_repo_url: Option<String>,
    pub render_branch: String,
    pub render_region: String,
    pub orvyn_studio_url: Option<String>,
    pub orvyn_studio_gpu_url: Option<String>,
    pub orvyn_studio_key: Option<String>,
    pub orvyn_worlds_url: Option<String>,
    pub orvyn_worlds_key: Option<String>,
    pub orvyn_studio_public_url: Option<String>,
    pub orvyn_studio_gpu_public_url: Option<String>,
    pub render_agent_plan: String,
    pub render_agent_plan_pro: String,
    pub render_agent_plan_studio: String,
    pub render_agent_env_group_id: Option<String>,
}

/// Reads settings from a map of variables (the process environment, or a test's).
struct Reader<'a> {
    vars: &'a HashMap<String, String>,
    problems: Vec<String>,
}

impl Reader<'_> {
    fn opt(&self, k: &str) -> Option<String> {
        self.vars.get(k).filter(|v| !v.is_empty()).cloned()
    }
    fn or(&self, k: &str, default: &str) -> String {
        self.opt(k).unwrap_or_else(|| default.to_string())
    }
    fn required(&mut self, k: &str, min_len: usize) -> String {
        match self.opt(k) {
            Some(v) if v.chars().count() >= min_len => v,
            Some(_) => {
                self.problems.push(format!("{k} must be at least {min_len} characters"));
                String::new()
            }
            None => {
                self.problems.push(format!("{k} is required"));
                String::new()
            }
        }
    }
    fn min_len(&mut self, k: &str, min_len: usize) -> Option<String> {
        let v = self.opt(k)?;
        if v.chars().count() < min_len {
            self.problems.push(format!("{k} must be at least {min_len} characters"));
        }
        Some(v)
    }
    fn matching(&mut self, k: &str, what: &str, ok: fn(&str) -> bool) -> Option<String> {
        let v = self.opt(k)?;
        if !ok(&v) {
            self.problems.push(format!("{k} must be {what}"));
        }
        Some(v)
    }
    fn address(&mut self, k: &str) -> Option<String> {
        self.matching(k, "an address (0x + 40 hex digits)", is_address)
    }
    fn address_or(&mut self, k: &str, default: &str) -> String {
        self.address(k).unwrap_or_else(|| default.to_string())
    }
    fn private_key(&mut self, k: &str) -> Option<String> {
        self.matching(k, "a private key (0x + 64 hex digits)", is_private_key)
    }
    fn url(&mut self, k: &str) -> Option<String> {
        self.matching(k, "a URL", |v| url::Url::parse(v).is_ok())
    }
    fn number(&mut self, k: &str, default: f64, min: f64, max: f64) -> f64 {
        match self.opt(k) {
            None => default,
            Some(v) => match v.trim().parse::<f64>() {
                Ok(n) if n >= min && n <= max => n,
                _ => {
                    self.problems.push(format!("{k} must be a number from {min} to {max}"));
                    default
                }
            },
        }
    }
    fn int(&mut self, k: &str, default: u32, min: u32, max: u32) -> u32 {
        match self.opt(k) {
            None => default,
            Some(v) => match v.trim().parse::<u32>() {
                Ok(n) if n >= min && n <= max => n,
                _ => {
                    self.problems.push(format!("{k} must be a whole number from {min} to {max}"));
                    default
                }
            },
        }
    }
    fn flag(&self, k: &str) -> bool {
        matches!(self.opt(k).as_deref(), Some("true") | Some("1"))
    }
}

pub fn is_address(v: &str) -> bool {
    v.len() == 42 && v.starts_with("0x") && v[2..].chars().all(|c| c.is_ascii_hexdigit())
}
pub fn is_private_key(v: &str) -> bool {
    v.len() == 66 && v.starts_with("0x") && v[2..].chars().all(|c| c.is_ascii_hexdigit())
}

impl Env {
    /// Parse settings from variables; every problem is reported at once.
    pub fn parse(vars: &HashMap<String, String>) -> Result<Env, String> {
        let mut r = Reader { vars, problems: vec![] };
        let database_url = match r.url("DATABASE_URL") {
            Some(v) => v,
            None => {
                r.problems.push("DATABASE_URL is required".into());
                String::new()
            }
        };
        let hosting_mode = match r.or("HOSTING_MODE", "shared").as_str() {
            "shared" => HostingMode::Shared,
            "render" => HostingMode::Render,
            "self" => HostingMode::SelfHosted,
            other => {
                r.problems.push(format!("HOSTING_MODE must be shared, render or self (not {other})"));
                HostingMode::Shared
            }
        };
        let erc8004_enabled = match r.or("ERC8004_ENABLED", "false").as_str() {
            "true" => true,
            "false" => false,
            _ => {
                r.problems.push("ERC8004_ENABLED must be true or false".into());
                false
            }
        };
        let pool = r.int("DATABASE_POOL_SIZE", 5, 1, 10_000);
        let e = Env {
            database_url,
            database_pool_size: pool,
            privy_app_id: r.opt("PRIVY_APP_ID"),
            privy_app_secret: r.opt("PRIVY_APP_SECRET"),
            privy_authorization_key: r.opt("PRIVY_AUTHORIZATION_KEY"),
            privy_authorization_key_id: r.opt("PRIVY_AUTHORIZATION_KEY_ID"),
            privy_allow_app_owned_wallets: r.opt("PRIVY_ALLOW_APP_OWNED_WALLETS").as_deref() == Some("true"),
            privy_verification_key: r.opt("PRIVY_VERIFICATION_KEY"),
            privy_agent_policy_id: r.opt("PRIVY_AGENT_POLICY_ID"),
            privy_aster_policy_id: r.opt("PRIVY_ASTER_POLICY_ID"),
            aster_api_url: r.or("ASTER_API_URL", "https://fapi.asterdex.com"),
            opinion_api_url: r.or("OPINION_API_URL", "https://openapi.opinion.trade/openapi"),
            opinion_api_key: r.opt("OPINION_API_KEY"),
            ai_key_encryption_secret: r.required("AI_KEY_ENCRYPTION_SECRET", 32),
            bsc_rpc_url: r.opt("BSC_RPC_URL"),
            session_secret: r.required("SESSION_SECRET", 32),
            orvyn_api_url: r.or("ORVYN_API_URL", "http://localhost:3100"),
            orvyn_internal_api_key: r.min_len("ORVYN_INTERNAL_API_KEY", 16),
            orvyn_api_keys: r.or("ORVYN_API_KEYS", ""),
            fourmeme_api_url: r.or("FOURMEME_API_URL", "https://four.meme/meme-api/v1"),
            fourmeme_token_manager: r.address_or("FOURMEME_TOKEN_MANAGER", "0x5c952063c7fc8610FFDB798152D69F0B9550762b"),
            fourmeme_helper: r.address_or("FOURMEME_HELPER", "0xF251F83e40a78868FcfA3FA4599Dad6494E46034"),
            flap_portal: r.address("FLAP_PORTAL"),
            tax_allocate_min_bnb: r.number("TAX_ALLOCATE_MIN_BNB", 0.005, 0.0, f64::MAX),
            orvyn_registry_contract: r.address("ORVYN_REGISTRY_CONTRACT"),
            orvyn_operator_private_key: r.private_key("ORVYN_OPERATOR_PRIVATE_KEY"),
            orvyn_rng_contract: r.address("ORVYN_RNG_CONTRACT"),
            orvyn_rng_relayer_key: r.private_key("ORVYN_RNG_RELAYER_KEY"),
            binance_pay_api_key: r.opt("BINANCE_PAY_API_KEY"),
            binance_pay_secret: r.opt("BINANCE_PAY_SECRET"),
            binance_pay_api_url: r.or("BINANCE_PAY_API_URL", "https://bpay.binanceapi.com"),
            binance_pay_webhook_url: r.opt("BINANCE_PAY_WEBHOOK_URL"),
            orvyn_binance_pay_creditor_key: r.private_key("ORVYN_BINANCE_PAY_CREDITOR_KEY"),
            billing_renew_days: r.number("BILLING_RENEW_DAYS", 3.0, 0.5, 30.0),
            billing_grace_hours: r.number("BILLING_GRACE_HOURS", 0.0, 0.0, 24.0 * 14.0),
            orvyn_attestation_key: r.private_key("ORVYN_ATTESTATION_KEY"),
            erc8004_enabled,
            erc8004_chain: r.or("ERC8004_CHAIN", "bsc"),
            erc8004_registry: r.address_or("ERC8004_REGISTRY", "0x8004A169FB4a3325136EB29fA0ceB6D2e539a432"),
            orvyn_erc8004_sponsor_key: r.private_key("ORVYN_ERC8004_SPONSOR_KEY"),
            orvyn_pay_contract: r.address("ORVYN_PAY_CONTRACT"),
            orvyn_shielded_pool: r.address("ORVYN_SHIELDED_POOL"),
            orvyn_admin_accounts: r.or("ORVYN_ADMIN_ACCOUNTS", ""),
            bliss_allow_private_urls: r.flag("BLISS_ALLOW_PRIVATE_URLS"),
            openrouter_api_key: r.opt("OPENROUTER_API_KEY"),
            orvyn_credits_address: r.address("ORVYN_CREDITS_ADDRESS"),
            inference_markup: r.number("ORVYN_INFERENCE_MARKUP", 1.2, 1.0, 5.0),
            orvyn_billing_entity: r.or("ORVYN_BILLING_ENTITY", "Orvyn"),
            orvyn_billing_address: r.opt("ORVYN_BILLING_ADDRESS"),
            orvyn_billing_email: r.opt("ORVYN_BILLING_EMAIL"),
            orvyn_billing_tax_id: r.opt("ORVYN_BILLING_TAX_ID"),
            orvyn_mail_domain: r.or("ORVYN_MAIL_DOMAIN", "orvyn.cash"),
            orvyn_mail_api_key: r.min_len("ORVYN_MAIL_API_KEY", 32),
            orvyn_mail_daily_send_cap: r.int("ORVYN_MAIL_DAILY_SEND_CAP", 100, 1, 1000),
            orvyn_mail_daily_receive_cap: r.int("ORVYN_MAIL_DAILY_RECEIVE_CAP", 500, 1, 10_000),
            orvyn_jobs_contract: r.address("ORVYN_JOBS_CONTRACT"),
            orvyn_jobs_operator_private_key: r.private_key("ORVYN_JOBS_OPERATOR_PRIVATE_KEY"),
            hosting_mode,
            render_api_key: r.opt("RENDER_API_KEY"),
            render_api_url: r.or("RENDER_API_URL", "https://api.render.com/v1"),
            render_owner_id: r.opt("RENDER_OWNER_ID"),
            render_repo_url: r.opt("RENDER_REPO_URL"),
            render_branch: r.or("RENDER_BRANCH", "main"),
            render_region: r.or("RENDER_REGION", "oregon"),
            orvyn_studio_url: r.url("ORVYN_STUDIO_URL"),
            orvyn_studio_gpu_url: r.url("ORVYN_STUDIO_GPU_URL"),
            orvyn_studio_key: r.min_len("ORVYN_STUDIO_KEY", 32),
            orvyn_worlds_url: r.url("ORVYN_WORLDS_URL"),
            orvyn_worlds_key: r.min_len("ORVYN_WORLDS_KEY", 32),
            orvyn_studio_public_url: r.url("ORVYN_STUDIO_PUBLIC_URL"),
            orvyn_studio_gpu_public_url: r.url("ORVYN_STUDIO_GPU_PUBLIC_URL"),
            render_agent_plan: r.or("RENDER_AGENT_PLAN", "starter"),
            render_agent_plan_pro: r.or("RENDER_AGENT_PLAN_PRO", "standard"),
            render_agent_plan_studio: r.or("RENDER_AGENT_PLAN_STUDIO", "pro"),
            render_agent_env_group_id: r.opt("RENDER_AGENT_ENV_GROUP_ID"),
        };
        if r.problems.is_empty() { Ok(e) } else { Err(r.problems.join("; ")) }
    }

    /// The BNB Chain RPC: BSC_RPC_URL, then EVM_RPC_BSC.
    pub fn bsc_rpc(&self) -> Option<&str> {
        self.bsc_rpc_url.as_deref()
    }
}

static ENV: OnceLock<Env> = OnceLock::new();

/// The settings, read on first use. Panics with every problem listed if they're wrong: a server
/// can't run on bad settings.
pub fn env() -> &'static Env {
    ENV.get_or_init(|| {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Env::parse(&vars).unwrap_or_else(|p| panic!("Orvyn settings: {p}"))
    })
}

/// Use these settings instead of the environment (tests, the self-hosted agent). Only before the
/// first `env()`.
pub fn set_env(e: Env) -> bool {
    ENV.set(e).is_ok()
}
