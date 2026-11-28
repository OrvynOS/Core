//! Vyness: coding agents on Orvyn AI.
//!
//! `vyness login` signs in with Orvyn (approve the code at app.orvyn.cash/device) and gets an Orvyn
//! AI key for this computer. Then `vyness claude`, `vyness opencode`, `vyness aider` or
//! `vyness run -- <command>` start the agent pointed at Orvyn AI; what it uses comes from your
//! Orvyn credits.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const CLIENT_ID: &str = "orv_vyness";
const SCOPE: &str = "openid profile inference";
const DEFAULT_API: &str = "https://api.orvyn.cash";
const TOP_UP: &str = "https://app.orvyn.cash/credits";

/// The open models Claude Code's tiers use on Orvyn AI (Opus: the strongest, Haiku: the fastest).
/// To run Claude Code on Claude itself, use your own Anthropic account with `claude` directly.
const OPUS: &str = "moonshotai/kimi-k3.1-pro";
const SONNET: &str = "z-ai/glm-5.3";
const HAIKU: &str = "deepseek/deepseek-v4.1-flash";

const HELP: &str = "Vyness: coding agents on Orvyn AI, paid from your Orvyn credits.

Usage:
  vyness login [--api <url>]     Sign in with Orvyn and get a key for this computer
  vyness claude [args...]        Start Claude Code on Orvyn AI
  vyness opencode [args...]      Start OpenCode on Orvyn AI
  vyness aider [args...]         Start Aider on Orvyn AI
  vyness run -- <command...>     Run any command with Orvyn AI's Anthropic and OpenAI settings
  vyness env                     Print those settings as shell exports
  vyness model [<id>|--reset]    Show or change your default model (DeepSeek V4.1 Flash unless you pick one)
  vyness models                  The models Orvyn AI serves
  vyness credits                 Your Orvyn credits
  vyness status                  Who is signed in on this computer
  vyness logout                  Forget the key on this computer

Agents start on your default model. Set --model (or VYNESS_MODEL) to use another for one run.";

// ---------- Config ----------

#[derive(Clone, Debug, PartialEq)]
struct Config {
    api: String,
    key: String,
    account: String,
}

fn config_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("VYNESS_HOME") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).ok_or_else(|| anyhow!("can't find your home folder"))?;
    Ok(PathBuf::from(home).join(".vyness"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

fn load() -> Result<Config> {
    let path = config_path()?;
    let raw = std::fs::read_to_string(&path).map_err(|_| anyhow!("Not signed in. Run `vyness login` first."))?;
    let v: Value = serde_json::from_str(&raw).with_context(|| format!("{} isn't valid JSON", path.display()))?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let c = Config { api: s("api"), key: s("key"), account: s("account") };
    if c.key.is_empty() || c.api.is_empty() {
        bail!("Not signed in. Run `vyness login` first.");
    }
    Ok(c)
}

fn save(c: &Config) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.json");
    let body = serde_json::to_string_pretty(&json!({ "api": c.api, "key": c.key, "account": c.account }))?;
    std::fs::write(&path, body)?;
    // The key is a secret: only this user may read it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn api_root(api: &str) -> String {
    api.trim().trim_end_matches('/').to_string()
}

// ---------- Settings for the agents ----------

/// Claude Code's Opus, Sonnet and Haiku tiers on the models Orvyn AI serves right now: each tier
/// falls back to the nearest one available (a model can be hidden while it's unavailable).
fn tiers(available: &[String]) -> [String; 3] {
    let has = |m: &str| available.iter().any(|a| a == m);
    let pick = |order: &[&str]| order.iter().find(|m| has(m)).map(|m| m.to_string()).or_else(|| available.first().cloned()).unwrap_or_else(|| order[0].to_string());
    [pick(&[OPUS, SONNET, HAIKU]), pick(&[SONNET, OPUS, HAIKU]), pick(&[HAIKU, SONNET, OPUS])]
}

/// The account's default model (set in Orvyn Chat or with `vyness model`).
async fn account_default(c: &Config) -> Option<String> {
    get_json(c, "/v1/default-model").await.ok().and_then(|v| v["model"].as_str().map(String::from))
}

async fn set_default(c: &Config, model: Option<&str>) -> Result<String> {
    let r = http()?.put(format!("{}/v1/default-model", api_root(&c.api))).bearer_auth(&c.key).json(&json!({ "model": model })).send().await?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(error_text(&v, status));
    }
    Ok(v["model"].as_str().unwrap_or_default().to_string())
}

async fn available_models(c: &Config) -> Vec<String> {
    get_json(c, "/v1/models").await.ok().and_then(|v| v["data"].as_array().map(|a| a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())).unwrap_or_default()
}

/// The environment that points a coding agent at Orvyn AI.
fn agent_env(c: &Config, model: Option<&str>) -> BTreeMap<String, String> {
    agent_env_with(c, model, &[OPUS.into(), SONNET.into(), HAIKU.into()])
}

fn agent_env_with(c: &Config, model: Option<&str>, tiers: &[String; 3]) -> BTreeMap<String, String> {
    let root = api_root(&c.api);
    let openai = format!("{root}/v1");
    let mut e = BTreeMap::new();
    let mut set = |k: &str, v: &str| {
        e.insert(k.to_string(), v.to_string());
    };
    // Claude Code (Anthropic Messages): it appends /v1/messages to the base URL.
    set("ANTHROPIC_BASE_URL", &root);
    set("ANTHROPIC_AUTH_TOKEN", &c.key);
    set("ANTHROPIC_API_KEY", "");
    set("ANTHROPIC_DEFAULT_OPUS_MODEL", &tiers[0]);
    set("ANTHROPIC_DEFAULT_SONNET_MODEL", &tiers[1]);
    set("ANTHROPIC_DEFAULT_HAIKU_MODEL", &tiers[2]);
    set("ANTHROPIC_SMALL_FAST_MODEL", &tiers[2]);
    if let Some(m) = model {
        set("ANTHROPIC_MODEL", m);
    }
    // OpenAI-compatible agents.
    set("OPENAI_BASE_URL", &openai);
    set("OPENAI_API_BASE", &openai);
    set("OPENAI_API_KEY", &c.key);
    set("ORVYN_API_KEY", &c.key);
    set("ORVYN_AI_BASE_URL", &openai);
    e
}

/// `--model x` or `--model=x` among the agent's arguments, else VYNESS_MODEL.
fn chosen_model(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--model" {
            return it.next().cloned();
        }
        if let Some(m) = a.strip_prefix("--model=") {
            return Some(m.to_string());
        }
    }
    std::env::var("VYNESS_MODEL").ok().filter(|m| !m.trim().is_empty())
}

/// OpenCode reads its providers from a config file; the key stays in the environment.
fn opencode_config(model: &str) -> Value {
    let models: serde_json::Map<String, Value> = [OPUS, SONNET, HAIKU, model].iter().map(|m| (m.to_string(), json!({ "name": m }))).collect();
    json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "orvyn": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Orvyn AI",
                "options": { "baseURL": "{env:ORVYN_AI_BASE_URL}", "apiKey": "{env:ORVYN_API_KEY}" },
                "models": models,
            }
        },
        "model": format!("orvyn/{model}"),
    })
}

/// Aider names OpenAI-compatible models "openai/<model>".
fn aider_args(args: &[String], model: &str) -> Vec<String> {
    let mut out = args.to_vec();
    if !args.iter().any(|a| a == "--model" || a.starts_with("--model=")) {
        out.splice(0..0, ["--model".to_string(), format!("openai/{model}")]);
    }
    out
}

/// Where the agents' own installers put them, for when that folder isn't on PATH.
fn known_locations(program: &str) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let mut out = Vec::new();
    let exe = if cfg!(windows) { format!("{program}.exe") } else { program.to_string() };
    if let Some(h) = &home {
        out.push(h.join(".local").join("bin").join(&exe));
        out.push(h.join(".claude").join("local").join(&exe));
        out.push(h.join(format!(".{program}")).join("bin").join(&exe));
        out.push(h.join(".npm-global").join("bin").join(program));
    }
    if cfg!(windows) {
        if let Some(a) = std::env::var_os("APPDATA") {
            out.push(PathBuf::from(a).join("npm").join(format!("{program}.cmd")));
        }
    }
    out.into_iter().filter(|p| p.is_file()).collect()
}

fn run_with(program: &str, args: &[String], env: &BTreeMap<String, String>) -> Result<ExitCode> {
    let spawn = |name: &std::ffi::OsStr| Command::new(name).args(args).envs(env).status();
    let mut status = spawn(program.as_ref());
    // npm installs Claude Code and OpenCode as .cmd shims on Windows.
    if cfg!(windows) && matches!(&status, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
        status = spawn(format!("{program}.cmd").as_ref());
    }
    for path in known_locations(program) {
        if !matches!(&status, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            break;
        }
        status = spawn(path.as_os_str());
    }
    match status {
        Ok(s) => Ok(ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("`{program}` isn't installed or isn't on your PATH.{}", install_hint(program)),
        Err(e) => Err(e.into()),
    }
}

fn install_hint(program: &str) -> &'static str {
    match program {
        "claude" if cfg!(windows) => " Install Claude Code (PowerShell): irm https://claude.ai/install.ps1 | iex   (or: npm install -g @anthropic-ai/claude-code)",
        "claude" => " Install Claude Code: curl -fsSL https://claude.ai/install.sh | bash   (or: npm install -g @anthropic-ai/claude-code)",
        "opencode" => " Install OpenCode: npm install -g opencode-ai",
        "aider" => " Install Aider: python -m pip install aider-install && aider-install",
        _ => "",
    }
}

// ---------- Orvyn ----------

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(concat!("vyness/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(30)).build()?)
}

fn error_text(v: &Value, status: reqwest::StatusCode) -> String {
    v["error_description"]
        .as_str()
        .or_else(|| v["error"]["message"].as_str())
        .or_else(|| v["error"].as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("Orvyn answered {status}"))
}

async fn get_json(c: &Config, path: &str) -> Result<Value> {
    let r = http()?.get(format!("{}{path}", api_root(&c.api))).bearer_auth(&c.key).send().await?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if status == 401 {
        bail!("This computer's key no longer works. Run `vyness login` again.");
    }
    if !status.is_success() {
        bail!(error_text(&v, status));
    }
    Ok(v)
}

fn device_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()))
        .unwrap_or_default();
    let name = name.trim();
    if name.is_empty() { "this computer".into() } else { name.chars().take(60).collect() }
}

fn open_browser(url: &str) {
    let _ = if cfg!(windows) {
        Command::new("cmd").args(["/C", "start", "", url]).status()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    };
}

async fn login(args: &[String]) -> Result<()> {
    let mut api = std::env::var("VYNESS_API").unwrap_or_else(|_| DEFAULT_API.into());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--api" => api = it.next().cloned().ok_or_else(|| anyhow!("--api needs a URL"))?,
            s if s.starts_with("--api=") => api = s["--api=".len()..].to_string(),
            "--no-browser" => {}
            other => bail!("unknown option {other}"),
        }
    }
    let api = api_root(&api);
    let client = http()?;

    // 1. A code to approve in the browser.
    let r = client.post(format!("{api}/api/oauth/device_authorization")).form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]).send().await?;
    let status = r.status();
    let start: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(error_text(&start, status));
    }
    let device_code = start["device_code"].as_str().ok_or_else(|| anyhow!("Orvyn didn't start the sign-in"))?.to_string();
    let user_code = start["user_code"].as_str().unwrap_or_default();
    let page = start["verification_uri_complete"].as_str().or_else(|| start["verification_uri"].as_str()).unwrap_or("https://app.orvyn.cash/device");
    let mut interval = start["interval"].as_u64().unwrap_or(5).max(1);
    let expires = start["expires_in"].as_u64().unwrap_or(600);

    println!("Sign in to Orvyn to use Orvyn AI with your credits.\n");
    println!("  Code: {user_code}");
    println!("  Open: {page}\n");
    if !args.iter().any(|a| a == "--no-browser") {
        open_browser(page);
    }
    println!("Waiting for you to approve...");

    // 2. Wait for the approval.
    let deadline = std::time::Instant::now() + Duration::from_secs(expires);
    let access = loop {
        if std::time::Instant::now() > deadline {
            bail!("The code expired. Run `vyness login` again.");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let r = client
            .post(format!("{api}/api/oauth/token"))
            .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", device_code.as_str()), ("client_id", CLIENT_ID)])
            .send()
            .await?;
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if let Some(t) = v["access_token"].as_str() {
            break t.to_string();
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => bail!("The sign-in was declined."),
            Some("expired_token") => bail!("The code expired. Run `vyness login` again."),
            _ => bail!(error_text(&v, status)),
        }
    };

    // 3. A key for this computer.
    let r = client.post(format!("{api}/api/vyness/key")).bearer_auth(&access).json(&json!({ "device": device_name() })).send().await?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!(error_text(&v, status));
    }
    let key = v["key"].as_str().ok_or_else(|| anyhow!("Orvyn didn't issue a key"))?.to_string();
    let account = v["account"].as_str().unwrap_or_default().to_string();
    let c = Config { api, key, account };
    save(&c)?;
    println!("\nSigned in as {}. Start an agent with `vyness claude`, `vyness opencode` or `vyness aider`.", short(&c.account));
    if let Ok(b) = get_json(&c, "/v1/credits").await {
        println!("Orvyn credits: {}", usd(&b["usd"]));
    }
    Ok(())
}

fn short(account: &str) -> String {
    if account.len() > 12 && account.starts_with("0x") { format!("{}…{}", &account[..6], &account[account.len() - 4..]) } else { account.to_string() }
}

fn usd(v: &Value) -> String {
    let n = v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);
    if n < 0.0 { format!("-${:.2}", -n) } else { format!("${n:.2}") }
}

async fn credits_low_warning(c: &Config) {
    if let Ok(b) = get_json(c, "/v1/credits").await {
        if b["usd"].as_f64().unwrap_or(0.0) <= 0.0 {
            eprintln!("vyness: you're out of Orvyn credits ({}). Top up at {TOP_UP}", usd(&b["usd"]));
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match argv.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => ("help", vec![]),
    };
    match cmd {
        "help" | "-h" | "--help" => println!("{HELP}"),
        "-V" | "--version" | "version" => println!("vyness {}", env!("CARGO_PKG_VERSION")),
        "login" => login(&rest).await?,
        "logout" => {
            let path = config_path()?;
            if path.exists() {
                std::fs::remove_file(&path)?;
                println!("Signed out. The key stays listed under API keys at https://app.orvyn.cash/credits until you revoke it there.");
            } else {
                println!("Not signed in.");
            }
        }
        "status" | "whoami" => {
            let c = load()?;
            println!("Signed in as {} on {}", short(&c.account), c.api);
        }
        "credits" => {
            let c = load()?;
            let b = get_json(&c, "/v1/credits").await?;
            println!("Orvyn credits: {}\nTop up: {}", usd(&b["usd"]), b["topUp"].as_str().unwrap_or(TOP_UP));
        }
        "model" => {
            let c = load()?;
            let m = match rest.first().map(String::as_str) {
                None => account_default(&c).await.ok_or_else(|| anyhow!("Couldn't read your default model."))?,
                Some("--reset") => set_default(&c, None).await?,
                Some(id) => set_default(&c, Some(id)).await?,
            };
            println!("Default model: {m}");
        }
        "models" => {
            let c = load()?;
            let v = get_json(&c, "/v1/models").await?;
            for m in v["data"].as_array().into_iter().flatten() {
                println!("{}", m["id"].as_str().unwrap_or_default());
            }
        }
        "env" => {
            let c = load()?;
            for (k, v) in agent_env(&c, chosen_model(&rest).as_deref()) {
                if cfg!(windows) { println!("$env:{k} = \"{v}\"") } else { println!("export {k}='{v}'") }
            }
        }
        "claude" => {
            let c = load()?;
            credits_low_warning(&c).await;
            let available = available_models(&c).await;
            let model = match chosen_model(&rest) {
                Some(m) => Some(m),
                None => account_default(&c).await,
            };
            let env = if available.is_empty() { agent_env(&c, model.as_deref()) } else { agent_env_with(&c, model.as_deref(), &tiers(&available)) };
            return run_with("claude", &rest, &env);
        }
        "opencode" => {
            let c = load()?;
            credits_low_warning(&c).await;
            let model = match chosen_model(&rest) {
                Some(m) => m,
                None => account_default(&c).await.unwrap_or_else(|| HAIKU.into()),
            };
            let path = config_dir()?.join("opencode.json");
            std::fs::write(&path, serde_json::to_string_pretty(&opencode_config(&model))?)?;
            let mut env = agent_env(&c, Some(&model));
            env.insert("OPENCODE_CONFIG".into(), path.to_string_lossy().into_owned());
            return run_with("opencode", &rest, &env);
        }
        "aider" => {
            let c = load()?;
            credits_low_warning(&c).await;
            let model = match chosen_model(&rest) {
                Some(m) => m,
                None => account_default(&c).await.unwrap_or_else(|| HAIKU.into()),
            };
            return run_with("aider", &aider_args(&rest, &model), &agent_env(&c, Some(&model)));
        }
        "run" => {
            let c = load()?;
            let cmdline: Vec<String> = rest.iter().skip_while(|a| *a == "--").cloned().collect();
            let Some((program, args)) = cmdline.split_first() else { bail!("Usage: vyness run -- <command...>") };
            return run_with(program, args, &agent_env(&c, chosen_model(args).as_deref()));
        }
        other => bail!("Unknown command `{other}`. Run `vyness help`."),
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("vyness: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config { api: "https://api.orvyn.cash/".into(), key: "orv_sk_test".into(), account: "0x4444444444444444444444444444444444444444".into() }
    }

    #[test]
    fn points_claude_code_and_openai_agents_at_orvyn_ai() {
        let e = agent_env(&cfg(), None);
        // Claude Code appends /v1/messages itself.
        assert_eq!(e["ANTHROPIC_BASE_URL"], "https://api.orvyn.cash");
        assert_eq!(e["ANTHROPIC_AUTH_TOKEN"], "orv_sk_test");
        assert_eq!(e["ANTHROPIC_API_KEY"], "", "an Anthropic key from the shell must not win");
        assert_eq!(e["ANTHROPIC_DEFAULT_HAIKU_MODEL"], HAIKU);
        assert!(!e.contains_key("ANTHROPIC_MODEL"));
        assert_eq!(e["OPENAI_BASE_URL"], "https://api.orvyn.cash/v1");
        assert_eq!(e["OPENAI_API_KEY"], "orv_sk_test");
        assert_eq!(agent_env(&cfg(), Some(OPUS))["ANTHROPIC_MODEL"], OPUS);
    }

    #[test]
    fn tiers_fall_back_to_available_models() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(tiers(&a(&[OPUS, SONNET, HAIKU])), [OPUS, SONNET, HAIKU].map(String::from));
        // Opus hidden: the Opus tier uses Sonnet.
        assert_eq!(tiers(&a(&[SONNET, HAIKU, "x/y"])), [SONNET, SONNET, HAIKU].map(String::from));
        // No Claude at all: the first model Orvyn AI serves.
        assert_eq!(tiers(&a(&["x/y", "z/w"])), ["x/y", "x/y", "x/y"].map(String::from));
    }

    #[test]
    fn reads_the_model_flag() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(chosen_model(&a(&["--model", "x/y", "-p", "hi"])).as_deref(), Some("x/y"));
        assert_eq!(chosen_model(&a(&["--model=x/z"])).as_deref(), Some("x/z"));
        assert_eq!(aider_args(&a(&["src/main.rs"]), HAIKU), a(&["--model", "openai/deepseek/deepseek-v4.1-flash", "src/main.rs"]));
        assert_eq!(aider_args(&a(&["--model", "openai/x"]), SONNET), a(&["--model", "openai/x"]));
    }

    #[test]
    fn opencode_keeps_the_key_out_of_its_config() {
        let v = opencode_config(SONNET);
        assert_eq!(v["model"], "orvyn/z-ai/glm-5.3");
        assert_eq!(v["provider"]["orvyn"]["options"]["apiKey"], "{env:ORVYN_API_KEY}");
        assert!(!v.to_string().contains("orv_sk"));
    }

    #[test]
    fn saves_and_loads_the_key() {
        let dir = std::env::temp_dir().join(format!("vyness-test-{}", std::process::id()));
        unsafe { std::env::set_var("VYNESS_HOME", &dir) };
        save(&cfg()).unwrap();
        assert_eq!(load().unwrap(), cfg());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
