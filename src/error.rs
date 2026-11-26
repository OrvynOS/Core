//! One error type for the core. The kind decides the HTTP status the API answers with; the message
//! is what the person sees (for `Internal`, it's logged and a generic message is shown).

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The request is wrong: 400.
    Invalid,
    /// Not signed in: 401.
    Unauthorized,
    /// Signed in, not allowed: 403.
    Forbidden,
    /// 404.
    NotFound,
    /// Conflicts with the current state (already exists, already done): 409.
    Conflict,
    /// Payment needed (x402, a plan): 402.
    PaymentRequired,
    /// Too many requests: 429.
    RateLimited,
    /// Something must be set up first (e.g. an AI key): 412.
    Precondition,
    /// A feature that isn't configured on this server: 503.
    Unavailable,
    /// Anything else: 500.
    Internal,
}

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new(Kind::Invalid, m)
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(Kind::Unauthorized, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(Kind::Forbidden, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(Kind::Conflict, m)
    }
    pub fn payment_required(m: impl Into<String>) -> Self {
        Self::new(Kind::PaymentRequired, m)
    }
    pub fn rate_limited(m: impl Into<String>) -> Self {
        Self::new(Kind::RateLimited, m)
    }
    pub fn precondition(m: impl Into<String>) -> Self {
        Self::new(Kind::Precondition, m)
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(Kind::Unavailable, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(Kind::Internal, m)
    }

    pub fn status(&self) -> u16 {
        match self.kind {
            Kind::Invalid => 400,
            Kind::Unauthorized => 401,
            Kind::PaymentRequired => 402,
            Kind::Forbidden => 403,
            Kind::NotFound => 404,
            Kind::Conflict => 409,
            Kind::RateLimited => 429,
            Kind::Precondition => 412,
            Kind::Unavailable => 503,
            Kind::Internal => 500,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Error::not_found("Not found."),
            other => Error::internal(format!("database: {other}")),
        }
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::internal(format!("{e:#}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::invalid(format!("bad JSON: {e}"))
    }
}

/// `bail!(Invalid, "...")`-style early return.
#[macro_export]
macro_rules! fail {
    ($kind:ident, $($arg:tt)*) => {
        return Err($crate::error::Error::new($crate::error::Kind::$kind, format!($($arg)*)))
    };
}
