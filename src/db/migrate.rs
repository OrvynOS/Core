//! Drizzle-compatible migrations: the same files, the same `drizzle.__drizzle_migrations` rows.

use sha2::{Digest, Sha256};
use sqlx::{Executor, PgPool, Row};

use crate::error::{Error, Result};

/// One migration from the journal.
pub struct Migration {
    pub tag: &'static str,
    /// The journal's `when`: drizzle applies every migration newer than the last one recorded.
    pub when: i64,
    pub sql: &'static str,
}

impl Migration {
    pub fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.sql.as_bytes()))
    }
    /// drizzle runs each part between `--> statement-breakpoint` markers as one statement.
    pub fn statements(&self) -> impl Iterator<Item = &'static str> {
        self.sql.split("--> statement-breakpoint").map(str::trim).filter(|s| !s.is_empty())
    }
}

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// Every migration, in journal order.
pub fn migrations() -> &'static [Migration] {
    MIGRATIONS
}

/// Apply the migrations the database doesn't have yet, in one transaction. Returns their tags.
pub async fn migrate(pool: &PgPool) -> Result<Vec<&'static str>> {
    pool.execute("CREATE SCHEMA IF NOT EXISTS \"drizzle\"").await?;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS \"drizzle\".\"__drizzle_migrations\" (id SERIAL PRIMARY KEY, hash text NOT NULL, created_at bigint)",
    )
    .await?;
    let last: Option<i64> = sqlx::query("select created_at from \"drizzle\".\"__drizzle_migrations\" order by created_at desc limit 1")
        .fetch_optional(pool)
        .await?
        .and_then(|r| r.try_get::<Option<i64>, _>(0).ok().flatten());
    let mut applied = vec![];
    let mut tx = pool.begin().await?;
    for m in MIGRATIONS {
        if last.is_some_and(|l| l >= m.when) {
            continue;
        }
        for stmt in m.statements() {
            sqlx::raw_sql(stmt)
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::internal(format!("migration {}: {e}", m.tag)))?;
        }
        sqlx::query("insert into \"drizzle\".\"__drizzle_migrations\" (\"hash\", \"created_at\") values ($1, $2)")
            .bind(m.hash())
            .bind(m.when)
            .execute(&mut *tx)
            .await?;
        applied.push(m.tag);
    }
    tx.commit().await?;
    Ok(applied)
}
