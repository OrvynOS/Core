//! The environment an agent runs in, published with its metadata: where it's hosted, its tier, the
//! hardware it runs on, and what it can render in Orvyn Studio. For Orvyn-hosted agents the
//! keccak256 of the canonical record is attested in the registry, so the bytes hashed must be the
//! TypeScript core's exactly (sorted keys, JavaScript number formatting).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::{db, Agent};
use crate::error::{Error, Result};
use crate::tiers::{categories_for, Tier};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Gpu {
    pub name: String,
    pub memory_gb: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hardware {
    pub cpu_model: String,
    pub cpus: f64,
    pub host_cpus: f64,
    pub memory_gb: f64,
    pub gpus: Vec<Gpu>,
    pub os: String,
    pub arch: String,
    pub container: bool,
    pub runtime: String,
}

impl Hardware {
    /// The same checks as the TypeScript core's hardwareSchema.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: &str| Err(Error::invalid(format!("hardware: {m}")));
        let len = |s: &str, n: usize| s.chars().count() <= n;
        if !len(&self.cpu_model, 120) || !len(&self.os, 120) || !len(&self.arch, 32) || !len(&self.runtime, 60) {
            return bad("text too long");
        }
        if !(self.cpus > 0.0 && self.cpus <= 4096.0 && self.host_cpus > 0.0 && self.host_cpus <= 4096.0) {
            return bad("cpus out of range");
        }
        if !(self.memory_gb > 0.0 && self.memory_gb <= 1_000_000.0) {
            return bad("memory out of range");
        }
        if self.gpus.len() > 64 || self.gpus.iter().any(|g| !len(&g.name, 120) || g.memory_gb.is_some_and(|m| !(0.0..=10_000.0).contains(&m))) {
            return bad("gpus out of range");
        }
        Ok(())
    }
}

fn round1(n: f64) -> f64 {
    (n * 10.0).round() / 10.0
}

/// Measure the machine this process runs on.
pub async fn probe_hardware() -> Hardware {
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(1.0);
    // cgroup CPU quota (v2, then v1).
    let quota = read("/sys/fs/cgroup/cpu.max")
        .and_then(|v| {
            let mut it = v.split_whitespace();
            let (q, p) = (it.next()?, it.next()?.parse::<f64>().ok()?);
            (q != "max" && p > 0.0).then(|| q.parse::<f64>().ok().map(|q| q / p)).flatten()
        })
        .or_else(|| {
            let q: f64 = read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")?.parse().ok()?;
            let p: f64 = read("/sys/fs/cgroup/cpu/cpu.cfs_period_us")?.parse().ok()?;
            (q > 0.0 && p > 0.0).then_some(q / p)
        });
    let total_mem = total_memory();
    let mem_limit = read("/sys/fs/cgroup/memory.max")
        .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|n| *n > 0.0 && *n < total_mem);
    let gpus = nvidia_gpus().await;
    let cpu_model = read("/proc/cpuinfo")
        .and_then(|c| c.lines().find(|l| l.starts_with("model name")).and_then(|l| l.split(':').nth(1)).map(|m| m.split_whitespace().collect::<Vec<_>>().join(" ")))
        .unwrap_or_else(|| "unknown".into());
    Hardware {
        cpu_model: crate::http::clip(&cpu_model, 120),
        cpus: quota.map(|q| round1(q.min(host_cpus))).unwrap_or(host_cpus),
        host_cpus,
        memory_gb: round1(mem_limit.unwrap_or(total_mem) / 1024f64.powi(3)),
        gpus,
        os: crate::http::clip(&format!("{} {}", std::env::consts::OS, read("/proc/sys/kernel/osrelease").unwrap_or_default()).trim().to_string(), 120),
        arch: std::env::consts::ARCH.into(),
        container: std::path::Path::new("/.dockerenv").exists() || quota.is_some() || mem_limit.is_some() || std::env::var("RENDER").is_ok() || std::env::var("KUBERNETES_SERVICE_HOST").is_ok(),
        runtime: format!("rust orvyn-core {}", env!("CARGO_PKG_VERSION")),
    }
}

fn total_memory() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|kb| kb.parse::<f64>().ok()))
        .map(|kb| kb * 1024.0)
        .unwrap_or(0.0)
}

async fn nvidia_gpus() -> Vec<Gpu> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        tokio::process::Command::new("nvidia-smi").args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"]).output(),
    )
    .await;
    let Ok(Ok(out)) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut parts = l.split(',').map(str::trim);
            let name = crate::http::clip(parts.next().unwrap_or(""), 120);
            let mb = parts.next().and_then(|m| m.parse::<f64>().ok());
            Gpu { name, memory_gb: mb.map(|m| round1(m / 1024.0)) }
        })
        .collect()
}

/// Two hardware reports describe the same machine (ignoring when they were measured).
pub fn same_hardware(a: Option<&Value>, b: &Hardware) -> bool {
    let Some(mut a) = a.cloned() else { return false };
    if let Some(o) = a.as_object_mut() {
        o.remove("measuredAt");
    }
    canonical_json(&a) == canonical_json(&serde_json::to_value(b).unwrap())
}

/// Store a hardware report (only when the machine changed, so restarts don't churn the attested
/// hash). Returns whether it changed.
pub async fn record_hardware(agent_id: &str, hardware: &Hardware) -> Result<bool> {
    hardware.validate()?;
    let row: Option<Option<Value>> = sqlx::query_scalar("select hardware from agents where id = $1").bind(agent_id).fetch_optional(db()).await?;
    let Some(current) = row else { return Ok(false) };
    if same_hardware(current.as_ref(), hardware) {
        return Ok(false);
    }
    let mut stored = serde_json::to_value(hardware)?;
    stored["measuredAt"] = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true).into();
    sqlx::query("update agents set hardware = $2 where id = $1").bind(agent_id).bind(stored).execute(db()).await?;
    Ok(true)
}

pub fn hosting_of(hosting_status: &str) -> &'static str {
    if hosting_status == "external" { "self-hosted" } else { "orvyn" }
}

/// The runtime plan name for a tier on this deployment.
pub fn plan_for(tier: Tier) -> String {
    let e = crate::env::env();
    let o = match tier {
        Tier::Basic => &e.render_agent_plan,
        Tier::Pro => &e.render_agent_plan_pro,
        Tier::Studio => &e.render_agent_plan_studio,
    };
    if o.trim().is_empty() { tier.spec().runtime.plan.to_string() } else { o.trim().to_string() }
}

/// The environment record (as JSON, the shape that's hashed).
pub fn environment_of(agent_id: &str, hosting_status: &str, tier: Tier, hardware: Option<&Value>) -> Value {
    let hosting = hosting_of(hosting_status);
    let spec = tier.spec();
    let s = &spec.studio;
    json!({
        "version": 1,
        "agentId": agent_id,
        "hosting": hosting,
        "tier": tier.as_str(),
        "plan": if hosting == "orvyn" { json!({ "name": plan_for(tier), "vcpu": spec.runtime.vcpu, "memoryGb": spec.runtime.memory_gb }) } else { Value::Null },
        "hardware": hardware.cloned().unwrap_or(Value::Null),
        "hardwareSource": match hardware { Some(_) if hosting == "orvyn" => json!("orvyn"), Some(_) => json!("self-reported"), None => Value::Null },
        "studio": {
            "renderers": s.renderers,
            "workers": s.workers,
            "maxRenderSeconds": s.max_render_seconds,
            "maxVideoSeconds": s.max_video_seconds,
            "maxResolution": format!("{}x{}", s.max_width, s.max_height),
            "maxOutputMb": s.max_output_mb,
        },
    })
}

pub fn environment_of_agent(agent: &Agent) -> Value {
    environment_of(&agent.id, &agent.hosting_status, Tier::parse(&agent.tier), agent.hardware.as_ref().filter(|h| !h.is_null()))
}

/// A JSON number as JavaScript prints it (1 not 1.0; 0.5; 1e21 and up in exponent form).
pub fn js_number_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        return format!("{}", f as i128);
    }
    let s = format!("{f}");
    s
}

/// JSON with sorted keys at every level and JavaScript number formatting: the exact bytes hashed.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical_json(&m[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        Value::Number(n) => js_number_str(n),
        other => other.to_string(),
    }
}

/// keccak256 of the canonical record: what the registry holds for Orvyn-hosted agents.
pub fn environment_hash(record: &Value) -> String {
    format!("{:#x}", alloy::primitives::keccak256(canonical_json(record).as_bytes()))
}

/// A short human line, e.g. "Pro · Orvyn-hosted · 2 vCPU, 4 GB · Studio CPU".
pub fn describe_environment(e: &Value) -> String {
    let n = |v: &Value| v.as_number().map(js_number_str).unwrap_or_default();
    let tier = Tier::parse(e["tier"].as_str().unwrap_or("basic"));
    let hw = &e["hardware"];
    let machine = if hw.is_object() {
        let cpus = hw["cpus"].as_f64().unwrap_or(0.0);
        let gpus: Vec<&str> = hw["gpus"].as_array().map(|g| g.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
        format!("{} CPU{}, {} GB{}", n(&hw["cpus"]), if cpus == 1.0 { "" } else { "s" }, n(&hw["memoryGb"]), if gpus.is_empty() { String::new() } else { format!(", {}", gpus.join(" + ")) })
    } else if e["plan"].is_object() {
        format!("{} vCPU, {} GB", n(&e["plan"]["vcpu"]), n(&e["plan"]["memoryGb"]))
    } else {
        "hardware not reported".into()
    };
    let hosting = if e["hosting"] == "orvyn" { "Orvyn-hosted" } else { "self-hosted" };
    format!("{} · {hosting} · {machine} · Studio {}", tier.spec().name, e["studio"]["workers"].as_str().unwrap_or("").to_uppercase())
}

/// The environment as published: the record, its hash, whether Orvyn attested it, a summary and the
/// job categories the tier allows.
pub fn public_environment(agent: &Agent) -> Value {
    let record = environment_of_agent(agent);
    let hash = environment_hash(&record);
    let mut out = record.clone();
    out["hash"] = hash.clone().into();
    out["attested"] = (record["hosting"] == "orvyn" && agent.environment_hash.as_deref() == Some(hash.as_str())).into();
    out["summary"] = describe_environment(&record).into();
    out["jobCategories"] = categories_for(Tier::parse(&agent.tier)).iter().map(|c| c.id).collect::<Vec<_>>().into();
    out
}
