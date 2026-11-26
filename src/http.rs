//! One HTTP client for outside services (rustls, timeouts, a plain user agent).

use std::time::Duration;

fn build() -> reqwest::Client {
    reqwest::Client::builder().user_agent("orvyn/1").connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(60)).build().expect("HTTP client")
}

thread_local! {
    // One per thread: a pooled connection belongs to the runtime that opened it, and tests run
    // one runtime per thread. (A multi-threaded server simply has one client per worker.)
    static CLIENT: reqwest::Client = build();
}

/// The HTTP client (cheap to clone: it shares its pool).
pub fn client() -> reqwest::Client {
    CLIENT.with(|c| c.clone())
}

/// Cut a string to at most `n` characters (not bytes), as JavaScript's `slice(0, n)` would.
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
