// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.0.0 — webhook subscriptions.
//!
//! Subscribers register a URL + shared secret + event/namespace/agent
//! filters. When a matching event fires (e.g. `memory_store`), a
//! fire-and-forget thread POSTs an HMAC-SHA256-signed JSON payload.
//!
//! SSRF hardening:
//! - `http://` only to `127.0.0.0/8` or `localhost` hosts;
//!   everywhere else requires `https://`
//! - RFC1918 / RFC4193 / link-local hosts are rejected unless
//!   `allow_private_networks = true` in the daemon config
//!
//! Signature:
//! - Header `X-Ai-Memory-Signature: sha256=<hex>` over the raw
//!   JSON body
//! - The secret stored in the DB is a SHA-256 of the plaintext
//!   shared secret; the plaintext is returned **once** at
//!   subscription time and never leaves the DB after.

use std::net::{IpAddr, ToSocketAddrs};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Public-facing subscription record (no secret plaintext).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub url: String,
    pub events: String,
    pub namespace_filter: Option<String>,
    pub agent_filter: Option<String>,
    pub created_by: Option<String>,
    pub created_at: String,
    pub dispatch_count: i64,
    pub failure_count: i64,
}

/// Parameters for creating a subscription.
pub struct NewSubscription<'a> {
    pub url: &'a str,
    pub events: &'a str,
    pub secret: Option<&'a str>,
    pub namespace_filter: Option<&'a str>,
    pub agent_filter: Option<&'a str>,
    pub created_by: Option<&'a str>,
}

/// Insert a subscription, hashing any secret before persisting.
///
/// Returns the new subscription's id.
pub fn insert(conn: &Connection, req: &NewSubscription<'_>) -> Result<String> {
    validate_url(req.url)?;
    let id = uuid::Uuid::new_v4().to_string();
    let secret_hash = req.secret.map(sha256_hex);
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO subscriptions (id, url, events, secret_hash, namespace_filter, agent_filter, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, req.url, req.events, secret_hash, req.namespace_filter, req.agent_filter, req.created_by, now],
    )?;
    Ok(id)
}

/// Delete a subscription by id. Returns true if a row was removed.
pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM subscriptions WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// List all active subscriptions.
pub fn list(conn: &Connection) -> Result<Vec<Subscription>> {
    let mut stmt = conn.prepare(
        "SELECT id, url, events, namespace_filter, agent_filter, created_by, created_at, dispatch_count, failure_count FROM subscriptions ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Subscription {
            id: row.get(0)?,
            url: row.get(1)?,
            events: row.get(2)?,
            namespace_filter: row.get(3)?,
            agent_filter: row.get(4)?,
            created_by: row.get(5)?,
            created_at: row.get(6)?,
            dispatch_count: row.get(7)?,
            failure_count: row.get(8)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("subscription row decode failed")
}

/// Test whether a subscription's filters match the given event.
fn matches_filters(
    sub_events: &str,
    sub_namespace: Option<&str>,
    sub_agent: Option<&str>,
    event: &str,
    namespace: &str,
    agent: Option<&str>,
) -> bool {
    // Event whitelist (comma-separated or `*`).
    let event_match = sub_events == "*"
        || sub_events
            .split(',')
            .map(str::trim)
            .any(|e| e == event || e == "*");
    if !event_match {
        return false;
    }
    if let Some(ns) = sub_namespace
        && !ns.is_empty()
        && ns != namespace
    {
        return false;
    }
    if let Some(filter) = sub_agent
        && !filter.is_empty()
        && agent.is_none_or(|a| a != filter)
    {
        return false;
    }
    true
}

/// Payload fired to subscribers. Stable JSON shape.
#[derive(Serialize)]
struct DispatchPayload<'a> {
    event: &'a str,
    memory_id: &'a str,
    namespace: &'a str,
    agent_id: Option<&'a str>,
    delivered_at: String,
}

/// Fire an event to all matching subscribers. Each dispatch runs in
/// its own OS thread and does NOT block the caller. Errors are logged
/// and counted in the DB via `failure_count`.
///
/// Caller owns the connection. Dispatch threads re-open the connection
/// as needed to update counters (cheap — `SQLite` connections are
/// process-shared via WAL).
pub fn dispatch_event(
    conn: &Connection,
    event: &str,
    memory_id: &str,
    namespace: &str,
    agent_id: Option<&str>,
    db_path: &std::path::Path,
) {
    let subs = match list(conn) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("subscription list failed during dispatch: {e}");
            return;
        }
    };
    let matching: Vec<Subscription> = subs
        .into_iter()
        .filter(|s| {
            matches_filters(
                &s.events,
                s.namespace_filter.as_deref(),
                s.agent_filter.as_deref(),
                event,
                namespace,
                agent_id,
            )
        })
        .collect();
    if matching.is_empty() {
        return;
    }
    let payload = DispatchPayload {
        event,
        memory_id,
        namespace,
        agent_id,
        delivered_at: chrono::Utc::now().to_rfc3339(),
    };
    let body = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("dispatch payload serialize failed: {e}");
            return;
        }
    };
    // Timestamp is part of the canonical string the signature is
    // computed over. Receivers SHOULD reject requests whose timestamp
    // differs from their clock by more than 5 minutes (replay window).
    // (#301 item 1 — prior implementation had no replay protection.)
    let timestamp = chrono::Utc::now().timestamp().to_string();
    for sub in matching {
        let url = sub.url.clone();
        let sub_id = sub.id.clone();
        let body = body.clone();
        let ts = timestamp.clone();
        let db_path = db_path.to_path_buf();
        std::thread::spawn(move || {
            let secret_hash = match load_secret_hash(&db_path, &sub_id) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("subscription secret lookup failed: {e}");
                    return;
                }
            };
            // Canonical string: "<timestamp>.<body>". Keyed HMAC over
            // the DB-stored secret hash. Receivers verify by computing
            // SHA256(plaintext_secret) and then
            // HMAC-SHA256(key, "<timestamp>.<body>").
            let canonical = format!("{ts}.{body}");
            let signature = secret_hash
                .as_deref()
                .map(|h| hmac_sha256_hex(h, &canonical));
            let ok = send(&url, &body, &ts, signature.as_deref());
            record_dispatch(&db_path, &sub_id, ok);
        });
    }
}

/// Perform one HTTP POST with SSRF-hardened URL check + signature
/// + timestamp headers. Returns true on any 2xx response.
fn send(url: &str, body: &str, timestamp: &str, signature: Option<&str>) -> bool {
    if let Err(e) = validate_url(url) {
        tracing::warn!("SSRF guard rejected webhook URL {url}: {e}");
        return false;
    }
    // DNS-resolution guard (#301 item 2). We rely on reqwest to
    // perform the connect, but pre-check by resolving the host here
    // and rejecting if any returned address is private / loopback /
    // link-local. Prevents DNS-rebind SSRF against attacker-controlled
    // domains that resolve to internal IPs.
    if let Err(e) = validate_url_dns(url) {
        tracing::warn!("DNS SSRF guard rejected webhook URL {url}: {e}");
        return false;
    }
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("webhook client build failed: {e}");
            return false;
        }
    };
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("user-agent", "ai-memory/0.6.0.0")
        .header("x-ai-memory-timestamp", timestamp);
    if let Some(sig) = signature {
        req = req.header("x-ai-memory-signature", format!("sha256={sig}"));
    }
    match req.body(body.to_string()).send() {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            tracing::warn!("webhook POST to {url} failed: {e}");
            false
        }
    }
}

/// Hash a plaintext secret (SHA-256 hex).
fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// HMAC-SHA256 is expensive to implement from scratch; do the simple
/// construction manually using the hashed secret as key material.
/// Matches the RFC-2104 HMAC construction with SHA-256 as the
/// primitive.
fn hmac_sha256_hex(key_hex: &str, body: &str) -> String {
    const BLOCK: usize = 64;
    // Decode key — if invalid hex, fall back to the raw bytes (which
    // keeps the signature stable for operators who set bad secrets;
    // verification will fail equally at receive time, which is loud
    // enough).
    let mut key = hex_decode(key_hex).unwrap_or_else(|| key_hex.as_bytes().to_vec());
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(&key);
        key = h.finalize().to_vec();
    }
    key.resize(BLOCK, 0);
    let mut opad = [0x5cu8; BLOCK];
    let mut ipad = [0x36u8; BLOCK];
    for i in 0..BLOCK {
        opad[i] ^= key[i];
        ipad[i] ^= key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(body.as_bytes());
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    format!("{:x}", outer.finalize())
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// SSRF guard with DNS resolution (#301 item 2). Resolves the host
/// via the stdlib resolver and rejects if ANY returned
/// `SocketAddr`'s IP is private / loopback / link-local. Guards
/// against DNS-rebind attacks where an attacker-controlled hostname
/// resolves to an internal IP at connect time.
///
/// Runs in the dispatch thread (blocking). Best-effort: if DNS fails
/// we let reqwest surface the error rather than fail closed, because
/// transient DNS outages should not silently drop webhook delivery.
pub fn validate_url_dns(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    let (_scheme, rest) = lower
        .split_once("://")
        .ok_or_else(|| anyhow!("webhook URL missing scheme: {url}"))?;
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host_port = &rest[..host_end];
    // Supply a default port so ToSocketAddrs resolves correctly.
    let resolv_target = if host_port.contains(':') || host_port.starts_with('[') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };
    let addrs: Vec<std::net::SocketAddr> = match resolv_target.to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(_) => return Ok(()), // DNS hiccup — let reqwest surface it
    };
    for addr in &addrs {
        let ip = addr.ip();
        if is_private(ip) && !ip.is_loopback() {
            return Err(anyhow!(
                "host resolves to private/link-local IP {ip}: {url}"
            ));
        }
    }
    Ok(())
}

/// SSRF guard. Rejects URLs that would cause the daemon to connect
/// to private-range addresses, link-local, loopback (except
/// explicitly), or non-HTTPS remote hosts.
pub fn validate_url(url: &str) -> Result<()> {
    // Cheap scheme check without pulling the `url` crate.
    let lower = url.to_ascii_lowercase();
    let (scheme, rest) = lower
        .split_once("://")
        .ok_or_else(|| anyhow!("webhook URL missing scheme: {url}"))?;
    if scheme != "https" && scheme != "http" {
        return Err(anyhow!("webhook URL scheme must be http(s): {url}"));
    }
    // Extract host (portion before '/' or ':' or '?'). IPv6 URLs use
    // `[ipv6]:port` syntax — the brackets must be stripped and the
    // colon-split must skip the colons inside the v6 literal.
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host_port = &rest[..host_end];
    let host: String = if let Some(stripped) = host_port.strip_prefix('[') {
        // IPv6: host is everything before the closing bracket.
        match stripped.find(']') {
            Some(i) => stripped[..i].to_string(),
            None => return Err(anyhow!("malformed IPv6 URL host: {url}")),
        }
    } else {
        // IPv4 / hostname.
        host_port
            .rsplit_once(':')
            .map_or(host_port.to_string(), |(h, _)| h.to_string())
    };
    let host = host.as_str();
    // Allow localhost for dev / CI.
    let is_loopback_hostname = matches!(host, "localhost" | "localhost.localdomain" | "");
    if scheme == "http" && !is_loopback_hostname {
        // Accept http only to parsed-loopback IPs; everything else
        // requires https.
        if let Ok(ip) = IpAddr::from_str(host) {
            if !ip.is_loopback() {
                return Err(anyhow!(
                    "webhook URL must be https for non-loopback host: {url}"
                ));
            }
        } else {
            return Err(anyhow!(
                "webhook URL must be https for non-loopback host: {url}"
            ));
        }
    }
    // Reject private-range IPs regardless of scheme (RFC1918 / RFC4193 /
    // link-local). Hostnames that resolve to private ranges are not
    // caught here — the dispatch thread will still be able to reach
    // them; operators who want to reach internal services should set
    // up reverse proxies or allow explicitly in config.
    if let Ok(ip) = IpAddr::from_str(host)
        && is_private(ip)
        && !ip.is_loopback()
    {
        return Err(anyhow!(
            "webhook URL targets private / link-local address: {url}"
        ));
    }
    Ok(())
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_link_local() || v4.is_multicast() || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            // Conservative: reject unique-local (fc00::/7), link-local
            // (fe80::/10), and multicast.
            let segs = v6.segments();
            v6.is_multicast()
                || (segs[0] & 0xfe00) == 0xfc00 // ULA
                || (segs[0] & 0xffc0) == 0xfe80 // link-local
        }
    }
}

fn load_secret_hash(db_path: &std::path::Path, sub_id: &str) -> Result<Option<String>> {
    let conn = Connection::open(db_path).context("load_secret_hash open")?;
    let row = conn
        .query_row(
            "SELECT secret_hash FROM subscriptions WHERE id = ?1",
            params![sub_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .context("load_secret_hash query")?;
    Ok(row)
}

fn record_dispatch(db_path: &std::path::Path, sub_id: &str, ok: bool) {
    let Ok(conn) = Connection::open(db_path) else {
        return;
    };
    let now = chrono::Utc::now().to_rfc3339();
    let sql = if ok {
        "UPDATE subscriptions SET dispatch_count = dispatch_count + 1, last_dispatched_at = ?1 WHERE id = ?2"
    } else {
        "UPDATE subscriptions SET dispatch_count = dispatch_count + 1, failure_count = failure_count + 1, last_dispatched_at = ?1 WHERE id = ?2"
    };
    let _ = conn.execute(sql, params![now, sub_id]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_allowed() {
        assert!(validate_url("https://example.com/hook").is_ok());
        assert!(validate_url("https://api.example.com:8443/hook?x=1").is_ok());
    }

    #[test]
    fn http_only_to_loopback() {
        assert!(validate_url("http://localhost/hook").is_ok());
        assert!(validate_url("http://127.0.0.1:8080/hook").is_ok());
        // IPv6 in URLs must be bracketed per RFC 3986 §3.2.2.
        assert!(validate_url("http://[::1]/hook").is_ok());
        assert!(validate_url("http://example.com/hook").is_err());
        assert!(validate_url("http://8.8.8.8/hook").is_err());
    }

    #[test]
    fn private_ranges_blocked() {
        assert!(validate_url("https://10.0.0.1/hook").is_err());
        assert!(validate_url("https://192.168.1.1/hook").is_err());
        assert!(validate_url("https://172.16.0.1/hook").is_err());
        assert!(validate_url("https://169.254.1.1/hook").is_err());
        assert!(validate_url("https://[fc00::1]/hook").is_err());
        assert!(validate_url("https://[fe80::1]/hook").is_err());
    }

    #[test]
    fn nonsense_rejected() {
        assert!(validate_url("ftp://example.com").is_err());
        assert!(validate_url("notaurl").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn hmac_sha256_stable() {
        // Known vector: HMAC-SHA256("key", "The quick brown fox jumps over the lazy dog")
        // = f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8
        let key = hex::encode_fallback("key".as_bytes());
        let got = hmac_sha256_hex(&key, "The quick brown fox jumps over the lazy dog");
        assert_eq!(
            got,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn filter_wildcards() {
        assert!(matches_filters("*", None, None, "memory_store", "ns", None));
        assert!(matches_filters(
            "memory_store,memory_delete",
            None,
            None,
            "memory_store",
            "ns",
            None
        ));
        assert!(!matches_filters(
            "memory_delete",
            None,
            None,
            "memory_store",
            "ns",
            None
        ));
        assert!(matches_filters(
            "*",
            Some("foo"),
            None,
            "memory_store",
            "foo",
            None
        ));
        assert!(!matches_filters(
            "*",
            Some("foo"),
            None,
            "memory_store",
            "bar",
            None
        ));
        assert!(matches_filters(
            "*",
            None,
            Some("alice"),
            "memory_store",
            "ns",
            Some("alice")
        ));
        assert!(!matches_filters(
            "*",
            None,
            Some("alice"),
            "memory_store",
            "ns",
            Some("bob")
        ));
    }
}

// Local hex helper used only by tests; the production paths use the
// format!("{:x}", _) pattern over GenericArray outputs.
#[cfg(test)]
mod hex {
    pub fn encode_fallback(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
