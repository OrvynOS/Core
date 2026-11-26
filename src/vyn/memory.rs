//! What Vyn remembers about a person: short notes she writes herself (remember) or the person adds,
//! shown back to her in every conversation. The person can read and delete every one of them.

use crate::db::{db, VynMemory};
use crate::error::{Error, Result};

const MAX_MEMORIES: usize = 300;
/// How many recent memories go into each conversation (older ones are found with recall).
pub const PROMPT_MEMORIES: i64 = 60;
pub const KINDS: &[&str] = &["fact", "preference", "person", "routine", "other"];

pub async fn list_memories(account_id: &str, limit: i64) -> Result<Vec<VynMemory>> {
    Ok(sqlx::query_as("select * from vyn_memories where account_id = $1 order by id desc limit $2").bind(account_id.to_lowercase()).bind(limit).fetch_all(db()).await?)
}

pub async fn add_memory(account_id: &str, text: &str, kind: &str) -> Result<VynMemory> {
    let account = account_id.to_lowercase();
    let clean: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect();
    if clean.is_empty() {
        return Err(Error::invalid("nothing to remember"));
    }
    let kind = if KINDS.contains(&kind) { kind } else { "fact" };
    let same: Option<VynMemory> = sqlx::query_as("select * from vyn_memories where account_id = $1 and text = $2").bind(&account).bind(&clean).fetch_optional(db()).await?;
    if let Some(s) = same {
        return Ok(s);
    }
    let n: i64 = sqlx::query_scalar("select count(*) from vyn_memories where account_id = $1").bind(&account).fetch_one(db()).await?;
    if n as usize >= MAX_MEMORIES {
        return Err(Error::invalid(format!("memory is full ({MAX_MEMORIES} notes); ask the person which ones to forget")));
    }
    Ok(sqlx::query_as("insert into vyn_memories (account_id, text, kind) values ($1, $2, $3) returning *").bind(&account).bind(&clean).bind(kind).fetch_one(db()).await?)
}

pub async fn forget_memory(account_id: &str, id: i64) -> Result<bool> {
    let n = sqlx::query("delete from vyn_memories where id = $1 and account_id = $2").bind(id as i32).bind(account_id.to_lowercase()).execute(db()).await?.rows_affected();
    Ok(n > 0)
}

pub async fn recall_memories(account_id: &str, query: &str) -> Result<Vec<VynMemory>> {
    let words: Vec<String> = query.split_whitespace().filter(|w| w.chars().count() > 2).take(5).map(|w| w.replace(['%', '_', '\\'], "")).collect();
    let mut seen: Vec<VynMemory> = vec![];
    for w in words {
        let rows: Vec<VynMemory> = sqlx::query_as("select * from vyn_memories where account_id = $1 and text ilike $2 limit 20").bind(account_id.to_lowercase()).bind(format!("%{w}%")).fetch_all(db()).await?;
        for r in rows {
            if !seen.iter().any(|s| s.id == r.id) {
                seen.push(r);
            }
        }
    }
    seen.truncate(30);
    Ok(seen)
}

/// The memory block for the system prompt.
pub fn memory_block(memories: &[VynMemory]) -> String {
    if memories.is_empty() {
        return "You don't know anything about them yet. When they tell you something worth keeping, remember it.".into();
    }
    memories.iter().map(|m| format!("- [{}] {}", m.id, m.text)).collect::<Vec<_>>().join("\n")
}
