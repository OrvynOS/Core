//! The four venue servers, each its own MCP endpoint (/<venue>/mcp).

pub mod flap;
pub mod fourmeme;
pub mod pancakeswap;
pub mod uniswap;

use serde_json::Value;

use crate::chain::Res;
use crate::tools::{Args, Tool};

pub const VENUES: &[&str] = &["pancakeswap", "uniswap", "fourmeme", "flap"];

pub struct Venue {
    pub name: &'static str,
    pub title: &'static str,
    pub instructions: &'static str,
    pub tools: Vec<Tool>,
}

pub fn venue(id: &str) -> Option<Venue> {
    Some(match id {
        "pancakeswap" => Venue { name: pancakeswap::NAME, title: pancakeswap::TITLE, instructions: pancakeswap::INSTRUCTIONS, tools: pancakeswap::tools() },
        "uniswap" => Venue { name: uniswap::NAME, title: uniswap::TITLE, instructions: uniswap::INSTRUCTIONS, tools: uniswap::tools() },
        "fourmeme" => Venue { name: fourmeme::NAME, title: fourmeme::TITLE, instructions: fourmeme::INSTRUCTIONS, tools: fourmeme::tools() },
        "flap" => Venue { name: flap::NAME, title: flap::TITLE, instructions: flap::INSTRUCTIONS, tools: flap::tools() },
        _ => return None,
    })
}

pub async fn call(venue: &str, tool: &str, args: &Args<'_>) -> Res<Value> {
    match venue {
        "pancakeswap" => pancakeswap::call(tool, args).await,
        "uniswap" => uniswap::call(tool, args).await,
        "fourmeme" => fourmeme::call(tool, args).await,
        _ => flap::call(tool, args).await,
    }
}
