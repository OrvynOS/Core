//! Startup and shutdown shared by Orvyn's binaries (API, orchestrator, agent): the monorepo's
//! root .env, settings from the process environment, and the stop signal.

use std::collections::HashMap;

use crate::env::{set_env, Env};

/// Load the first of `paths` that exists (`KEY=value` lines; `#` comments; quotes trimmed).
/// Variables already set win, as with Node's `process.loadEnvFile`. Call before starting threads.
pub fn load_dotenv(paths: &[&str]) {
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))).unwrap_or(v);
                if !k.is_empty() && std::env::var_os(k).is_none() {
                    // SAFETY: called at startup, before any other thread exists.
                    unsafe { std::env::set_var(k, v) };
                }
            }
        }
        return;
    }
}

/// Parse and install the settings from the process environment.
pub fn init_env() -> anyhow::Result<()> {
    let vars: HashMap<String, String> = std::env::vars().collect();
    set_env(Env::parse(&vars).map_err(|e| anyhow::anyhow!("Orvyn settings: {e}"))?);
    Ok(())
}

/// Resolves on Ctrl-C, or SIGTERM (Render's deploy signal) on Unix.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
