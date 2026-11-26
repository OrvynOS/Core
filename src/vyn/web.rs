//! read_web for Vyn: fetch a public web page on the server and return its readable text, for
//! devices without Vyn's browser. Only public http(s) addresses: every address the host resolves
//! to is checked, the connection is pinned to the checked address (so DNS can't be swapped between
//! check and use), every redirect hop is checked again, and private, loopback and link-local
//! addresses are refused.

use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

const MAX_BYTES: usize = 2_000_000;
const MAX_TEXT: usize = 8000;

fn private_v4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 0 || a == 10 || a == 127 || (a == 100 && (64..=127).contains(&b)) || (a == 169 && b == 254) || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 192 && b == 0) || (a == 198 && (b == 18 || b == 19)) || a >= 224
}

pub fn is_private_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return private_v4(v4);
            }
            let s = v6.segments();
            v6.is_unspecified() || v6.is_loopback() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

fn check_url(url: &url::Url) -> Result<(), String> {
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err("only http and https pages".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("no credentials in URLs".into());
    }
    let host = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".internal") || host.ends_with(".local") {
        return Err("that address isn't public".into());
    }
    if host.parse::<IpAddr>().is_ok_and(is_private_address) {
        return Err("that address isn't public".into());
    }
    Ok(())
}

/// Resolve the host; every answer must be public. Returns the address to connect to.
async fn public_address(url: &url::Url) -> Result<SocketAddr, String> {
    let host = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| e.to_string())?.collect();
    if addrs.is_empty() || addrs.iter().any(|a| is_private_address(a.ip())) {
        return Err("that address isn't public".into());
    }
    Ok(addrs[0])
}

macro_rules! re {
    ($s:expr) => {
        LazyLock::new(|| Regex::new($s).unwrap())
    };
}
static BLOCKS: LazyLock<Regex> = re!(r"(?is)<(script|style|noscript|svg|template)\b.*?</(script|style|noscript|svg|template)>");
static COMMENTS: LazyLock<Regex> = re!(r"(?s)<!--.*?-->");
static BLOCK_END: LazyLock<Regex> = re!(r"(?i)</(p|div|li|h[1-6]|tr|section|article)>");
static BR: LazyLock<Regex> = re!(r"(?i)<br\s*/?>");
static TAGS: LazyLock<Regex> = re!(r"<[^>]+>");
static SPACES: LazyLock<Regex> = re!(r"[ \t\x0c\x0b]+");
static LINES: LazyLock<Regex> = re!(r"\s*\n\s*");
static TITLE: LazyLock<Regex> = re!(r"(?is)<title[^>]*>(.*?)</title>");

/// Strip a page down to readable text.
pub fn page_text(html: &str) -> String {
    // The regex crate has no backreferences: drop each block whose closing tag matches its opener.
    let mut s = String::with_capacity(html.len());
    let mut last = 0;
    for c in BLOCKS.captures_iter(html) {
        let m = c.get(0).unwrap();
        if c[1].eq_ignore_ascii_case(&c[2]) {
            s.push_str(&html[last..m.start()]);
            s.push(' ');
            last = m.end();
        }
    }
    s.push_str(&html[last..]);
    let s = COMMENTS.replace_all(&s, " ");
    let s = BLOCK_END.replace_all(&s, "\n");
    let s = BR.replace_all(&s, "\n");
    let s = TAGS.replace_all(&s, " ");
    let s = s.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
    let s = SPACES.replace_all(&s, " ");
    LINES.replace_all(&s, "\n").trim().to_string()
}

pub async fn read_web(raw: &str) -> Result<Value, String> {
    let mut url = url::Url::parse(raw).map_err(|_| "not a URL".to_string())?;
    for _ in 0..4 {
        check_url(&url)?;
        let addr = public_address(&url).await?;
        let host = url.host_str().unwrap_or("").to_string();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve(&host, addr)
            .timeout(Duration::from_secs(12))
            .user_agent("VynBot/1.0 (+https://orvyn.cash)")
            .build()
            .map_err(|e| e.to_string())?;
        let res = client.get(url.clone()).header("accept", "text/html,text/plain;q=0.9,*/*;q=0.5").header("accept-encoding", "identity").send().await.map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            if let Some(loc) = res.headers().get("location").and_then(|l| l.to_str().ok()) {
                url = url.join(loc).map_err(|_| "bad redirect".to_string())?;
                continue;
            }
        }
        if !(200..300).contains(&status) {
            return Err(format!("the page answered {status}"));
        }
        let kind = res.headers().get("content-type").and_then(|t| t.to_str().ok()).unwrap_or("").to_string();
        if !(kind.contains("text/") || kind.contains("html") || kind.contains("json") || kind.contains("xml")) {
            return Err(format!("not a text page ({})", if kind.is_empty() { "unknown type" } else { &kind }));
        }
        let mut body = Vec::new();
        let mut res = res;
        while let Some(chunk) = res.chunk().await.map_err(|e| e.to_string())? {
            if body.len() + chunk.len() > MAX_BYTES {
                break;
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&body).into_owned();
        let title = TITLE.captures(&body).map(|c| c[1].trim().to_string()).unwrap_or_default();
        let text = if kind.contains("html") { page_text(&body) } else { body };
        let n = text.encode_utf16().count();
        return Ok(json!({ "url": url.to_string(), "title": page_text(&title), "text": text.chars().take(MAX_TEXT).collect::<String>(), "truncated": n > MAX_TEXT }));
    }
    Err("too many redirects".into())
}
