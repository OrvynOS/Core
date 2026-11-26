//! Signing in with email or a social account, through Privy.
//!
//! The web app and Orvyn Mobile use Privy's SDKs to sign the person in. On first sign-in Privy
//! creates their embedded EVM wallet, which they hold (Orvyn can't sign with it). The client sends
//! Privy's identity token here; we verify it against our Privy app's keys, and it carries the
//! person's linked accounts (their embedded wallet, email, X or Telegram).
//!
//! The embedded wallet's address is the person's Orvyn identity. People with their own wallet sign
//! in with SIWE instead (auth.rs); both end up as the same kind of account, an EVM address.

use serde::Serialize;
use serde_json::{json, Value};

use crate::auth::create_session;
use crate::db::{db, User};
use crate::error::{Error, Result};
use crate::evm::is_address;

/// What a verified Privy identity token says about the person.
#[derive(Debug, Clone, PartialEq)]
pub struct PrivyPerson {
    pub privy_user_id: String,
    pub account_id: String,
    pub email: Option<String>,
    pub social_provider: Option<String>,
    pub social_username: Option<String>,
}

/// Read the person from an identity token's user id and linked accounts.
pub fn person_from_linked(user_id: &str, accounts: &[Value]) -> Result<PrivyPerson> {
    let field = |a: &Value, k: &str| a.get(k).and_then(Value::as_str).map(String::from);
    let of_type = |t: &str| accounts.iter().find(|a| a.get("type").and_then(Value::as_str) == Some(t));
    // Privy's embedded wallet: a "wallet" on ethereum whose client is privy.
    let wallet = accounts.iter().find(|a| {
        field(a, "type").as_deref() == Some("wallet") && field(a, "chain_type").as_deref() == Some("ethereum") && field(a, "wallet_client_type").as_deref() == Some("privy")
    });
    let address = wallet.and_then(|w| field(w, "address")).filter(|a| is_address(a));
    let Some(address) = address else {
        return Err(Error::conflict("Your Orvyn wallet is still being created. Try again in a moment."));
    };
    let email = of_type("email").and_then(|a| field(a, "address")).or_else(|| of_type("google_oauth").and_then(|a| field(a, "email")));
    let social = of_type("twitter_oauth").or_else(|| of_type("telegram"));
    Ok(PrivyPerson {
        privy_user_id: user_id.to_string(),
        account_id: address.to_lowercase(),
        email,
        social_provider: social.map(|s| if field(s, "type").as_deref() == Some("twitter_oauth") { "twitter".into() } else { "telegram".into() }),
        social_username: social.and_then(|s| field(s, "username")),
    })
}

pub async fn verify_privy_token(identity_token: &str) -> Result<PrivyPerson> {
    let user = crate::privy::verify_identity_token(identity_token)
        .await
        .map_err(|_| Error::unauthorized("Your sign-in expired or couldn't be verified. Sign in again."))?;
    if user.id.is_empty() {
        return Err(Error::unauthorized("Sign-in token has no user."));
    }
    person_from_linked(&user.id, &user.linked_accounts)
}

#[derive(Debug)]
pub struct PrivySignIn {
    pub user: User,
    pub token: String,
    pub first_sign_in: bool,
}

/// Sign in with a Privy identity token: records the person (first time: their new wallet) and
/// returns a session.
pub async fn sign_in_with_privy(identity_token: &str) -> Result<PrivySignIn> {
    let person = verify_privy_token(identity_token).await?;
    record_privy_person(&person).await
}

pub async fn record_privy_person(person: &PrivyPerson) -> Result<PrivySignIn> {
    // One Privy user, one Orvyn account: refuse if this user is linked to another account already.
    let existing: Option<User> = sqlx::query_as("select * from users where privy_user_id = $1").bind(&person.privy_user_id).fetch_optional(db()).await?;
    if existing.as_ref().is_some_and(|u| u.account_id != person.account_id) {
        return Err(Error::conflict("This sign-in is linked to a different Orvyn wallet. Contact support."));
    }
    let user: User = sqlx::query_as(
        "insert into users (account_id, sign_in_method, privy_user_id, email, social_provider, social_username, last_sign_in_at)
         values ($1, 'privy', $2, $3, $4, $5, now())
         on conflict (account_id) do update set privy_user_id = excluded.privy_user_id, email = excluded.email,
           social_provider = excluded.social_provider, social_username = excluded.social_username, last_sign_in_at = now()
         returning *",
    )
    .bind(&person.account_id)
    .bind(&person.privy_user_id)
    .bind(&person.email)
    .bind(&person.social_provider)
    .bind(&person.social_username)
    .fetch_one(db())
    .await?;
    let token = create_session(&user.account_id);
    Ok(PrivySignIn { user, token, first_sign_in: existing.is_none() })
}

/// Wallet (SIWE) sign-ins are recorded too (no wallet is created: they bring their own).
pub async fn record_wallet_sign_in(account_id: &str) -> Result<()> {
    sqlx::query(
        "insert into users (account_id, sign_in_method, last_sign_in_at) values ($1, 'wallet', now())
         on conflict (account_id) do update set last_sign_in_at = now()",
    )
    .bind(account_id.to_lowercase())
    .execute(db())
    .await?;
    Ok(())
}

pub async fn get_user(account_id: &str) -> Result<Option<User>> {
    Ok(sqlx::query_as("select * from users where account_id = $1").bind(account_id.to_lowercase()).fetch_optional(db()).await?)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserView {
    pub account_id: String,
    pub sign_in_method: String,
    pub email: Option<String>,
    pub social: Option<Value>,
    pub wallet: Value,
    pub created_at: Option<i64>,
}

/// The profile a signed-in person sees about themselves.
pub fn user_view(user: Option<&User>, account_id: &str) -> UserView {
    UserView {
        account_id: account_id.to_string(),
        sign_in_method: user.map(|u| u.sign_in_method.clone()).unwrap_or_else(|| "wallet".into()),
        email: user.and_then(|u| u.email.clone()),
        social: user.and_then(|u| u.social_provider.as_ref().map(|p| json!({ "provider": p, "username": u.social_username }))),
        wallet: json!({ "bsc": account_id }),
        created_at: user.map(|u| u.created_at.timestamp_millis()),
    }
}
