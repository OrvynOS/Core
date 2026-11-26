//! orvyn-admin: Orvyn's operator tool. Run where the settings are (the orchestrator, or a checkout
//! with the root .env):
//!
//!   orvyn-admin api-client <client-id> [--execute]       issue an API key to an outside app
//!   orvyn-admin hold <agent id | handle> "<reason>"      pause an agent on Orvyn's side
//!   orvyn-admin hold <agent id | handle> --release       release that pause
//!   orvyn-admin wallet-check <agent id | handle> | --new can an agent's wallet sign?
//!   orvyn-admin privy-policy [--print]                   create the Privy wallet policies
//!   orvyn-admin preview-db [port]                        a throwaway local database with demo agents
//!   orvyn-admin new-migration <name>                     add an empty migration to crates/core
//!   orvyn-admin fixture orvynpay [out-dir]               contracts/test/fixtures/deposit.json
//!   orvyn-admin fixture rng [out-dir]                    contracts/test/fixtures/rng.json

mod fixtures;
mod ops;
mod preview;

use anyhow::{bail, Result};

const USAGE: &str = "orvyn-admin api-client <client-id> [--execute]
orvyn-admin hold <agent id | handle> \"<reason>\" | --release
orvyn-admin wallet-check <agent id | handle> | --new
orvyn-admin privy-policy [--print]
orvyn-admin preview-db [port]
orvyn-admin new-migration <name>
orvyn-admin fixture orvynpay|rng [out-dir]";

/// Proving (fixture orvynpay) needs more stack than a main thread gets on Windows (1 MB): run on
/// a thread, and runtime workers, with room to spare.
fn main() -> Result<()> {
    orvyn_core::service::load_dotenv(&["../../.env", ".env"]);
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(64 * 1024 * 1024).build()?.block_on(run()))?
        .join()
        .map_err(|_| anyhow::anyhow!("orvyn-admin panicked"))?
}

async fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(0) {
        Some("api-client") => ops::api_client(arg(1), args.iter().any(|a| a == "--execute")).await,
        Some("hold") => ops::hold(arg(1), arg(2)).await,
        Some("wallet-check") => ops::wallet_check(arg(1)).await,
        Some("privy-policy") => ops::privy_policy(args.iter().any(|a| a == "--print")).await,
        Some("preview-db") => preview::run(arg(1).and_then(|p| p.parse().ok()).unwrap_or(5433)).await,
        Some("new-migration") => ops::new_migration(arg(1)),
        Some("fixture") => match arg(1) {
            Some("orvynpay") => fixtures::orvynpay(arg(2)).await,
            Some("rng") => fixtures::rng(arg(2)).await,
            _ => bail!("usage: orvyn-admin fixture orvynpay|rng [out-dir]"),
        },
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}
