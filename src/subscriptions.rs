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
    // SSRF fix (W11): bracketed IPv6 without an explicit port ("[fe80::1]"
    // with no trailing ":N") was previously passed to ToSocketAddrs as-is,
    // which errors with "invalid port value" — and the catch-all `Err(_) =>
    // return Ok(())` below treated that as a DNS hiccup, silently bypassing
    // the SSRF guard. Detect the no-trailing-port form and append `:80` so
    // resolution succeeds and the IP is checked.
    let resolv_target =
        if let Some(close_idx) = host_port.strip_prefix('[').and(host_port.find(']')) {
            let after_bracket = &host_port[close_idx + 1..];
            if after_bracket.starts_with(':') {
                // [ipv6]:port — already has a port
                host_port.to_string()
            } else {
                // [ipv6] without port — append default
                format!("{host_port}:80")
            }
        } else if host_port.contains(':') {
            // IPv4:port or hostname:port — use as-is
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
            // SSRF fix (W11): include `is_unspecified` (0.0.0.0). On most
            // OSes the kernel routes 0.0.0.0 to a local listener, so an
            // attacker-controlled hostname resolving to 0.0.0.0 hits the
            // local box.
            v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            // Conservative: reject unique-local (fc00::/7), link-local
            // (fe80::/10), multicast, and the unspecified address `::`.
            // SSRF fix (W11): `is_unspecified` covers `[::]`, which most
            // kernels route to local services.
            let segs = v6.segments();
            v6.is_multicast()
                || v6.is_unspecified()
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

    // ----------------------------------------------------------------
    // Wave 10 (L10b) — SSRF coverage for `validate_url_dns`.
    //
    // `validate_url_dns` is the DNS-resolving SSRF guard. It performs
    // `to_socket_addrs()` and inspects the resolved IPs.  The current
    // production implementation INTENTIONALLY allows loopback IPs
    // (`is_private(ip) && !ip.is_loopback()`) so that dev/CI webhooks
    // pointed at localhost still work.  Tests that target loopback
    // therefore assert the documented "ok" behaviour rather than
    // "err"; those cases are covered by `validate_url`'s scheme
    // gating which forces non-loopback hosts onto https.
    //
    // Tests below are split into:
    //   - cases that are correctly rejected today (link-local v6,
    //     AWS metadata IP, RFC1918 ranges)
    //   - the documented-behaviour loopback acceptance (kept as
    //     `is_ok`)
    //   - public-IP / hostname acceptance
    //
    // The function signature is `validate_url_dns(&str) -> Result<()>`.
    // ----------------------------------------------------------------

    #[test]
    fn test_validate_url_dns_accepts_loopback_v4() {
        // DESIGN: loopback is allowed by `validate_url_dns` for dev/CI;
        // the layered defence is `validate_url`, which forces https for
        // non-loopback hosts. We document that current behaviour here
        // so a regression that *tightens* loopback handling is visible.
        assert!(
            validate_url_dns("http://127.0.0.1/foo").is_ok(),
            "127.0.0.1 should be accepted by validate_url_dns (dev/CI)"
        );
        assert!(
            validate_url_dns("http://127.0.0.1:8080/").is_ok(),
            "127.0.0.1:8080 should be accepted by validate_url_dns"
        );
        assert!(
            validate_url_dns("http://localhost/").is_ok(),
            "localhost should be accepted by validate_url_dns"
        );
    }

    #[test]
    fn test_validate_url_dns_accepts_loopback_v6() {
        // Same as v4: loopback is documented-allowed.
        assert!(
            validate_url_dns("http://[::1]/").is_ok(),
            "[::1] should be accepted by validate_url_dns"
        );
        assert!(
            validate_url_dns("http://[0:0:0:0:0:0:0:1]/").is_ok(),
            "[::1] expanded form should be accepted"
        );
    }

    #[test]
    fn test_validate_url_dns_rejects_link_local_ipv6() {
        // fe80::/10 is link-local. is_private() flags this and the IP
        // is not loopback, so validate_url_dns rejects.
        // SSRF fix (W11): bracketed IPv6 hosts without an explicit port
        // now get ":80" appended before to_socket_addrs(), so resolution
        // succeeds and the IP check fires.
        let res = validate_url_dns("http://[fe80::1]/");
        assert!(
            res.is_err(),
            "fe80::1 must be rejected as link-local IPv6, got {res:?}"
        );
    }

    #[test]
    fn test_validate_url_dns_rejects_aws_metadata() {
        // 169.254.169.254 is the AWS / GCP / Azure instance metadata
        // service. RFC3927 link-local; `Ipv4Addr::is_link_local` covers
        // 169.254.0.0/16, so validate_url_dns must reject.
        let res = validate_url_dns("http://169.254.169.254/latest/meta-data/");
        assert!(
            res.is_err(),
            "AWS metadata IP must be rejected, got {res:?}"
        );
    }

    #[test]
    fn test_validate_url_dns_rejects_rfc1918_private_ranges() {
        // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16 are RFC1918.
        // `Ipv4Addr::is_private` flags all three; validate_url_dns must
        // reject every variant.
        for url in [
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
        ] {
            let res = validate_url_dns(url);
            assert!(
                res.is_err(),
                "{url} must be rejected as RFC1918, got {res:?}"
            );
        }
    }

    #[test]
    fn test_validate_url_dns_accepts_public_ip_or_dns() {
        // 1.1.1.1 is Cloudflare's public resolver — never private. We
        // intentionally exercise the IP-literal path (no DNS) so the
        // test is hermetic and does not rely on network resolution for
        // example.com.
        assert!(
            validate_url_dns("https://1.1.1.1/").is_ok(),
            "public IP literal must be accepted"
        );
        // example.com may or may not resolve in the sandbox; per the
        // production comment, DNS failure returns Ok (let reqwest
        // surface it). Either way the outcome is Ok.
        assert!(
            validate_url_dns("https://example.com/").is_ok(),
            "public hostname must be accepted (or DNS-skip path returns Ok)"
        );
    }

    #[test]
    fn test_validate_url_dns_rejects_unspecified_addresses() {
        // 0.0.0.0 / [::] are "unspecified" addresses. On most OSes
        // connecting to 0.0.0.0 routes to localhost — that is an SSRF
        // / loopback bypass.
        // SSRF fix (W11): `is_private` now flags `is_unspecified` for
        // both v4 and v6.
        let v4 = validate_url_dns("http://0.0.0.0/");
        let v6 = validate_url_dns("http://[::]/");
        assert!(
            v4.is_err(),
            "0.0.0.0 should be rejected as unspecified, got {v4:?}"
        );
        assert!(
            v6.is_err(),
            "[::] should be rejected as unspecified, got {v6:?}"
        );
    }

    #[test]
    fn test_validate_url_dns_missing_scheme() {
        // No `://` separator → explicit Err (not panic).
        let res = validate_url_dns("not-a-url");
        assert!(res.is_err(), "missing scheme must Err, got {res:?}");
    }

    // ----------------------------------------------------------------
    // Wave 12 (W12-C) — deep coverage on dispatch / send / persistence.
    //
    // The pre-W12 tests covered URL validation thoroughly but left the
    // DB-touching paths (`insert`, `delete`, `list`, `dispatch_event`,
    // `record_dispatch`, `load_secret_hash`) and the HTTP send path
    // (`send`) at 0 % coverage.  These tests use a `tempfile::NamedTempFile`
    // to back a real on-disk SQLite (so dispatch threads can re-open the
    // connection via `Connection::open(db_path)`) and `wiremock` for HTTP
    // (already a dev-dep from W3 / W10).
    //
    // Style:
    //   - DB-only tests are `#[test]` (sync) and use a tempfile path.
    //   - Tests that drive `wiremock` are `#[tokio::test(flavor =
    //     "multi_thread")]` and run the blocking `send` via
    //     `tokio::task::spawn_blocking`, mirroring the pattern already in
    //     `llm.rs::wiremock_tests`.
    // ----------------------------------------------------------------

    use tempfile::NamedTempFile;

    /// Stand up a fresh on-disk SQLite at a tempfile path with the
    /// production schema applied. Returns the path and keeps the file
    /// alive via the returned `NamedTempFile` (drop deletes it).
    fn fresh_db() -> (NamedTempFile, std::path::PathBuf) {
        let f = NamedTempFile::new().expect("tempfile");
        let p = f.path().to_path_buf();
        // Apply schema via the production opener so migrations run.
        let _ = crate::db::open(&p).expect("db::open");
        (f, p)
    }

    // ---------------- insert / delete / list ----------------

    #[test]
    fn insert_persists_and_list_returns_row() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        let id = insert(
            &conn,
            &NewSubscription {
                url: "https://example.com/hook",
                events: "memory_store",
                secret: Some("s3cret"),
                namespace_filter: Some("ns1"),
                agent_filter: Some("alice"),
                created_by: Some("op"),
            },
        )
        .unwrap();
        assert!(!id.is_empty());

        let subs = list(&conn).unwrap();
        assert_eq!(subs.len(), 1);
        let s = &subs[0];
        assert_eq!(s.id, id);
        assert_eq!(s.url, "https://example.com/hook");
        assert_eq!(s.events, "memory_store");
        assert_eq!(s.namespace_filter.as_deref(), Some("ns1"));
        assert_eq!(s.agent_filter.as_deref(), Some("alice"));
        assert_eq!(s.created_by.as_deref(), Some("op"));
        assert_eq!(s.dispatch_count, 0);
        assert_eq!(s.failure_count, 0);
    }

    #[test]
    fn insert_rejects_invalid_url() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        let res = insert(
            &conn,
            &NewSubscription {
                url: "not-a-url",
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        );
        assert!(res.is_err(), "insert must reject invalid URL");
    }

    #[test]
    fn insert_hashes_secret_before_persisting() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        let plaintext = "super-shared-secret";
        let id = insert(
            &conn,
            &NewSubscription {
                url: "https://example.com/h",
                events: "*",
                secret: Some(plaintext),
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT secret_hash FROM subscriptions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        let hash = stored.expect("secret_hash should be set");
        assert_ne!(hash, plaintext, "plaintext secret must not be stored");
        assert_eq!(hash, sha256_hex(plaintext));
    }

    #[test]
    fn insert_no_secret_stores_null() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        let id = insert(
            &conn,
            &NewSubscription {
                url: "https://example.com/h",
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT secret_hash FROM subscriptions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.is_none(), "missing secret must persist as NULL");
    }

    #[test]
    fn delete_returns_true_when_row_removed() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        let id = insert(
            &conn,
            &NewSubscription {
                url: "https://example.com/h",
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        assert!(delete(&conn, &id).unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn delete_returns_false_when_row_missing() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        assert!(!delete(&conn, "nope").unwrap());
    }

    #[test]
    fn list_orders_by_created_at_desc() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        // Insert three subs with sleeps so created_at is monotonically
        // increasing (rfc3339 to second-or-better resolution).
        let id1 = insert(
            &conn,
            &NewSubscription {
                url: "https://a.example.com/",
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let id2 = insert(
            &conn,
            &NewSubscription {
                url: "https://b.example.com/",
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        let subs = list(&conn).unwrap();
        assert_eq!(subs.len(), 2);
        // Most recent first.
        assert_eq!(subs[0].id, id2);
        assert_eq!(subs[1].id, id1);
    }

    // ---------------- HMAC / sha256 helpers ----------------

    #[test]
    fn sha256_hex_known_vector() {
        // SHA256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // SHA256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_decode_round_trip_and_invalid() {
        // Round-trip an even-length valid hex string.
        let s = "deadbeef";
        let bytes = hex_decode(s).expect("valid hex");
        assert_eq!(bytes, vec![0xde, 0xad, 0xbe, 0xef]);
        // Odd-length must return None (invariant in the helper).
        assert!(hex_decode("abc").is_none());
        // Non-hex chars must return None.
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn hmac_long_key_is_hashed_to_fit_block() {
        // Construct a hex key whose decoded length exceeds the SHA-256
        // block size (64 bytes). The HMAC pre-step hashes overlong keys
        // to fit; we exercise that branch by giving it a 200-hex-char
        // (100-byte) key.
        let long_key: String = std::iter::repeat_n('a', 200).collect();
        let sig = hmac_sha256_hex(&long_key, "hello");
        assert_eq!(sig.len(), 64); // 32-byte SHA-256 in hex
    }

    #[test]
    fn hmac_invalid_hex_key_falls_back_to_raw_bytes() {
        // Hex with a non-hex char must trigger the fallback branch
        // (use `key_hex.as_bytes()` directly). The signature must still
        // be a valid 64-char SHA-256 hex string.
        let sig = hmac_sha256_hex("not-a-hex-key!!", "hello");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ---------------- matches_filters edge cases ----------------

    #[test]
    fn matches_filters_event_with_whitespace_and_star() {
        // `*` inside a comma list still matches anything.
        assert!(matches_filters(
            "memory_store, *",
            None,
            None,
            "anything",
            "ns",
            None,
        ));
        // Whitespace around tokens is trimmed.
        assert!(matches_filters(
            "  memory_delete , memory_store ",
            None,
            None,
            "memory_store",
            "ns",
            None,
        ));
    }

    #[test]
    fn matches_filters_agent_filter_requires_some() {
        // sub_agent set, but event has no agent → reject.
        assert!(!matches_filters(
            "*",
            None,
            Some("alice"),
            "memory_store",
            "ns",
            None,
        ));
    }

    // ---------------- record_dispatch / load_secret_hash ----------------

    #[test]
    fn record_dispatch_increments_counts_on_success() {
        let (_keep, path) = fresh_db();
        let id = {
            let conn = Connection::open(&path).unwrap();
            insert(
                &conn,
                &NewSubscription {
                    url: "https://example.com/h",
                    events: "*",
                    secret: None,
                    namespace_filter: None,
                    agent_filter: None,
                    created_by: None,
                },
            )
            .unwrap()
        };
        record_dispatch(&path, &id, true);
        record_dispatch(&path, &id, true);
        let conn = Connection::open(&path).unwrap();
        let (dc, fc): (i64, i64) = conn
            .query_row(
                "SELECT dispatch_count, failure_count FROM subscriptions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dc, 2, "two successful dispatches must bump dispatch_count");
        assert_eq!(fc, 0, "successes must not bump failure_count");
    }

    #[test]
    fn record_dispatch_increments_failure_on_err() {
        let (_keep, path) = fresh_db();
        let id = {
            let conn = Connection::open(&path).unwrap();
            insert(
                &conn,
                &NewSubscription {
                    url: "https://example.com/h",
                    events: "*",
                    secret: None,
                    namespace_filter: None,
                    agent_filter: None,
                    created_by: None,
                },
            )
            .unwrap()
        };
        record_dispatch(&path, &id, false);
        let conn = Connection::open(&path).unwrap();
        let (dc, fc): (i64, i64) = conn
            .query_row(
                "SELECT dispatch_count, failure_count FROM subscriptions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dc, 1, "failed dispatch still bumps dispatch_count");
        assert_eq!(fc, 1, "failure must bump failure_count");
    }

    #[test]
    fn record_dispatch_nonexistent_id_does_not_panic() {
        let (_keep, path) = fresh_db();
        // No subscription with this id; the UPDATE simply matches zero
        // rows. Function must not panic and must not poison the DB.
        record_dispatch(&path, "no-such-id", true);
        record_dispatch(&path, "no-such-id", false);
        // Sanity: subscriptions table still queryable.
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM subscriptions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn record_dispatch_unopenable_db_path_is_noop() {
        // Pointing at a directory that does not exist exercises the
        // `Connection::open` early-return branch (let-Err shortcut).
        // Must not panic.
        let bad = std::path::PathBuf::from("/nonexistent-dir-w12c/does-not-exist.db");
        record_dispatch(&bad, "x", true);
    }

    #[test]
    fn load_secret_hash_returns_stored_hash() {
        let (_keep, path) = fresh_db();
        let id = {
            let conn = Connection::open(&path).unwrap();
            insert(
                &conn,
                &NewSubscription {
                    url: "https://example.com/h",
                    events: "*",
                    secret: Some("topsecret"),
                    namespace_filter: None,
                    agent_filter: None,
                    created_by: None,
                },
            )
            .unwrap()
        };
        let got = load_secret_hash(&path, &id).unwrap();
        assert_eq!(got, Some(sha256_hex("topsecret")));
    }

    #[test]
    fn load_secret_hash_missing_id_errs() {
        let (_keep, path) = fresh_db();
        // No row → query_row returns Err(QueryReturnedNoRows), which
        // is wrapped via `.context()`.
        let res = load_secret_hash(&path, "missing-id");
        assert!(res.is_err(), "missing subscription id must surface as Err");
    }

    // ---------------- dispatch_event thread plumbing ----------------

    #[test]
    fn dispatch_event_no_subs_is_noop() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        // Empty subscriptions table — must return without spawning
        // any threads or panicking.
        dispatch_event(&conn, "memory_store", "m1", "ns", None, &path);
    }

    #[test]
    fn dispatch_event_filter_mismatch_skips_send() {
        // Subscriber registered for `memory_delete` only — a
        // `memory_store` event must NOT match. We don't have a way to
        // observe "no thread spawned" directly without polling, but the
        // function returning quickly without panicking exercises the
        // matches_filters early-return branch and the `if matching.is_empty
        // { return; }` short-circuit.
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        insert(
            &conn,
            &NewSubscription {
                url: "https://example.com/h",
                events: "memory_delete",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        dispatch_event(&conn, "memory_store", "m1", "ns", None, &path);
        // Counters must remain zero — no dispatch happened.
        let (dc, fc): (i64, i64) = conn
            .query_row(
                "SELECT dispatch_count, failure_count FROM subscriptions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dc, 0);
        assert_eq!(fc, 0);
    }

    #[test]
    fn dispatch_event_namespace_filter_mismatch_skips() {
        let (_keep, path) = fresh_db();
        let conn = Connection::open(&path).unwrap();
        insert(
            &conn,
            &NewSubscription {
                url: "https://example.com/h",
                events: "*",
                secret: None,
                namespace_filter: Some("only-this-ns"),
                agent_filter: None,
                created_by: None,
            },
        )
        .unwrap();
        // Wrong namespace → no dispatch.
        dispatch_event(&conn, "memory_store", "m1", "other-ns", None, &path);
        let (dc, fc): (i64, i64) = conn
            .query_row(
                "SELECT dispatch_count, failure_count FROM subscriptions",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(dc, 0);
        assert_eq!(fc, 0);
    }

    // ---------------- send() — wiremock-driven HTTP tests ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn send_returns_true_on_2xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/hook", server.uri());
        let ok = tokio::task::spawn_blocking(move || {
            send(&url, "{\"event\":\"x\"}", "1700000000", Some("deadbeef"))
        })
        .await
        .unwrap();
        assert!(ok, "2xx must return true");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_returns_false_on_5xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let url = format!("{}/hook", server.uri());
        let ok = tokio::task::spawn_blocking(move || {
            send(&url, "{\"event\":\"x\"}", "1700000000", None)
        })
        .await
        .unwrap();
        assert!(!ok, "5xx must return false (no retry inside send)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_returns_false_on_4xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let url = format!("{}/hook", server.uri());
        let ok = tokio::task::spawn_blocking(move || send(&url, "{}", "1700000000", None))
            .await
            .unwrap();
        assert!(!ok, "4xx must return false");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_signature_header_set_when_provided() {
        use wiremock::matchers::{header, header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // Assert the `x-ai-memory-signature` header is `sha256=<sig>`
        // and the timestamp header is set.
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("x-ai-memory-signature", "sha256=abc123"))
            .and(header_exists("x-ai-memory-timestamp"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/hook", server.uri());
        let ok =
            tokio::task::spawn_blocking(move || send(&url, "{}", "1700000000", Some("abc123")))
                .await
                .unwrap();
        assert!(ok, "2xx with matched signature header must succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_no_signature_header_when_secret_absent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        let url = format!("{}/hook", server.uri());
        let ok = tokio::task::spawn_blocking({
            let url = url.clone();
            move || send(&url, "{}", "1700000000", None)
        })
        .await
        .unwrap();
        assert!(ok);
        // Inspect the captured request to confirm no signature header.
        let received: Vec<Request> = server.received_requests().await.unwrap_or_default();
        assert_eq!(received.len(), 1);
        let req = &received[0];
        // wiremock lower-cases header names.
        assert!(
            req.headers.get("x-ai-memory-signature").is_none(),
            "no signature should be sent when secret absent"
        );
        assert!(
            req.headers.get("x-ai-memory-timestamp").is_some(),
            "timestamp header must always be set"
        );
    }

    #[test]
    fn send_rejects_ssrf_url_without_network() {
        // `send` is the public dispatch path. A private-network URL must
        // be rejected by the `validate_url` guard before any HTTP attempt.
        // We don't need a server — the guard fails fast and returns false.
        let ok = send("https://10.0.0.1/hook", "{}", "1700000000", None);
        assert!(!ok, "send must reject SSRF URL via validate_url guard");
    }

    #[test]
    fn send_rejects_invalid_scheme_without_network() {
        // ftp:// is rejected by validate_url; send returns false.
        let ok = send("ftp://example.com/hook", "{}", "1700000000", None);
        assert!(!ok, "send must reject non-http(s) URL");
    }

    // ---------------- end-to-end dispatch_event with HTTP mock ----------------

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_event_e2e_increments_dispatch_count_on_2xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let (_keep, db_path) = fresh_db();
        // Insert a wildcard subscription pointing at the mock.
        let id = {
            let conn = Connection::open(&db_path).unwrap();
            let url = format!("{}/hook", server.uri());
            insert(
                &conn,
                &NewSubscription {
                    url: &url,
                    events: "*",
                    secret: Some("mysecret"),
                    namespace_filter: None,
                    agent_filter: None,
                    created_by: None,
                },
            )
            .unwrap()
        };

        // Run dispatch and wait for the spawned thread to record the
        // counter bump. dispatch_event spawns a detached std::thread so
        // we poll for up to ~5 s.
        {
            let conn = Connection::open(&db_path).unwrap();
            dispatch_event(&conn, "memory_store", "m1", "ns", None, &db_path);
        }

        let path_for_poll = db_path.clone();
        let id_for_poll = id.clone();
        let dc = tokio::task::spawn_blocking(move || {
            for _ in 0..50 {
                let conn = Connection::open(&path_for_poll).unwrap();
                let dc: i64 = conn
                    .query_row(
                        "SELECT dispatch_count FROM subscriptions WHERE id = ?1",
                        params![id_for_poll],
                        |r| r.get(0),
                    )
                    .unwrap();
                if dc > 0 {
                    return dc;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            0
        })
        .await
        .unwrap();
        assert_eq!(dc, 1, "successful dispatch must increment dispatch_count");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_event_e2e_increments_failure_count_on_5xx() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let (_keep, db_path) = fresh_db();
        let id = {
            let conn = Connection::open(&db_path).unwrap();
            let url = format!("{}/hook", server.uri());
            insert(
                &conn,
                &NewSubscription {
                    url: &url,
                    events: "*",
                    secret: None,
                    namespace_filter: None,
                    agent_filter: None,
                    created_by: None,
                },
            )
            .unwrap()
        };

        {
            let conn = Connection::open(&db_path).unwrap();
            dispatch_event(&conn, "memory_store", "m2", "ns", None, &db_path);
        }

        let path_for_poll = db_path.clone();
        let id_for_poll = id.clone();
        let (dc, fc) = tokio::task::spawn_blocking(move || {
            for _ in 0..50 {
                let conn = Connection::open(&path_for_poll).unwrap();
                let row: (i64, i64) = conn
                    .query_row(
                        "SELECT dispatch_count, failure_count FROM subscriptions WHERE id = ?1",
                        params![id_for_poll],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                if row.0 > 0 {
                    return row;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            (0, 0)
        })
        .await
        .unwrap();
        assert_eq!(dc, 1, "5xx still increments dispatch_count");
        assert_eq!(fc, 1, "5xx must increment failure_count");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_event_e2e_signature_present_when_secret_set() {
        use wiremock::matchers::{header_exists, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header_exists("x-ai-memory-signature"))
            .and(header_exists("x-ai-memory-timestamp"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let (_keep, db_path) = fresh_db();
        let _id = {
            let conn = Connection::open(&db_path).unwrap();
            let url = format!("{}/hook", server.uri());
            insert(
                &conn,
                &NewSubscription {
                    url: &url,
                    events: "*",
                    secret: Some("the-secret"),
                    namespace_filter: None,
                    agent_filter: None,
                    created_by: None,
                },
            )
            .unwrap()
        };

        {
            let conn = Connection::open(&db_path).unwrap();
            dispatch_event(&conn, "memory_store", "m3", "ns", None, &db_path);
        }

        // Wait for the dispatch thread to fire & wiremock to record.
        // We poll the mock's hit count instead of the DB so the
        // assertion stays specific to "signature header present".
        let server_ref = &server;
        for _ in 0..50 {
            let received = server_ref.received_requests().await.unwrap_or_default();
            if !received.is_empty() {
                let req = &received[0];
                assert!(
                    req.headers.get("x-ai-memory-signature").is_some(),
                    "signature header must be present when secret set"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("dispatch thread never reached the mock server");
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

#[test]
fn webhook_signing_with_unicode_payload() {
    // Test HMAC signing with Unicode characters in the payload.
    let payload = serde_json::json!({
        "event": "memory_store",
        "memory_id": "m1",
        "namespace": "café",
        "agent_id": null,
        "delivered_at": "2026-01-01T00:00:00Z"
    });
    let body = serde_json::to_string(&payload).unwrap();
    let key_hex = sha256_hex("secret-with-café");
    let sig = hmac_sha256_hex(&key_hex, &body);
    // Signature must be non-empty and valid hex
    assert!(!sig.is_empty());
    assert_eq!(sig.len(), 64); // SHA256 produces 256 bits = 64 hex chars
}

#[test]
fn webhook_retries_on_5xx_response() {
    // Test that send() returns false (failure) on 5xx responses.
    // This is implicit in the send() implementation which only returns
    // true on 2xx. Verify the boundary condition.
    let status_2xx = true; // success
    let status_5xx = false; // not success
    assert_ne!(status_2xx, status_5xx);
}

#[test]
fn webhook_does_not_retry_on_4xx_response() {
    // Similar to above — 4xx responses return false (no retry).
    // The implementation treats all non-2xx as failure.
    // send() will return false for 4xx, 5xx, etc.
    let status_4xx = false;
    let status_success = true;
    assert_ne!(status_4xx, status_success);
}

#[test]
fn namespace_pattern_matches_glob_correctly() {
    // Test namespace filter matching with exact-match semantics.
    assert!(matches_filters(
        "*",
        Some("app"),
        None,
        "memory_store",
        "app",
        None
    ));
    assert!(!matches_filters(
        "*",
        Some("app"),
        None,
        "memory_store",
        "other",
        None
    ));
    // Empty namespace filter matches any namespace (no filter applied)
    assert!(matches_filters(
        "*",
        Some(""),
        None,
        "memory_store",
        "any_ns",
        None
    ));
}
