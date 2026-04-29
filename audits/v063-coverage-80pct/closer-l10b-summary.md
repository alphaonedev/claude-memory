# Closer L10b — Wave 10 Coverage Summary (subscriptions.rs SSRF)

**Branch:** `cov-90pct-w10/subscriptions-ssrf`
**Date:** 2026-04-26
**Owner:** Closer L10b
**Files:** `src/subscriptions.rs` (test-only appends inside `mod tests`)

## Coverage delta

| File              | Pre (W9 lines) | Post (W10 lines) | Δ      | Target |
|-------------------|---------------:|-----------------:|-------:|-------:|
| subscriptions.rs  | 69.45%         | **75.00%**       | +5.55  | n/a    |
| Codebase          | 84.42%         | **85.30%**       | +0.88  | n/a    |

## Tests added (8 total, all in `subscriptions::tests`)

The brief targeted `validate_url_dns` — the DNS-resolving SSRF guard.
The W9 baseline only exercised `validate_url` (the cheap-IP-literal
guard); the DNS-resolving path was 0 % covered.

1. `test_validate_url_dns_accepts_loopback_v4` — `127.0.0.1`,
   `127.0.0.1:8080`, `localhost`. The brief expected `Err`; production
   intentionally allows loopback for dev/CI (the layered defence is
   `validate_url`'s scheme gate). Test asserts the documented
   ALLOW behaviour so a regression that tightens loopback handling is
   visible.
2. `test_validate_url_dns_accepts_loopback_v6` — `[::1]`,
   `[0:0:0:0:0:0:0:1]`. Same documented-ALLOW behaviour.
3. `test_validate_url_dns_rejects_link_local_ipv6` — `[fe80::1]`.
   **Gated `#[ignore]` with FIXME — see Surprises (real SSRF gap).**
4. `test_validate_url_dns_rejects_aws_metadata` —
   `169.254.169.254/latest/meta-data/`. Properly rejected via
   `Ipv4Addr::is_link_local`.
5. `test_validate_url_dns_rejects_rfc1918_private_ranges` — `10.0.0.1`,
   `172.16.0.1`, `172.31.255.255`, `192.168.1.1`. All four properly
   rejected.
6. `test_validate_url_dns_accepts_public_ip_or_dns` — `1.1.1.1`,
   `example.com`. Hermetic w.r.t. DNS (production fallback is
   `Err(_) => Ok(())`).
7. `test_validate_url_dns_rejects_unspecified_addresses` — `0.0.0.0`,
   `[::]`. **Gated `#[ignore]` with FIXME — see Surprises (real SSRF
   gap).**
8. `test_validate_url_dns_missing_scheme` — explicit Err on missing
   `://`.

### Tests gated `#[ignore]`

- `test_validate_url_dns_rejects_link_local_ipv6`
  — *FIXME: validate_url_dns accepts `http://[fe80::1]/` because
  bracketed IPv6 hosts without an explicit port skip the `:80`
  default. `to_socket_addrs("[fe80::1]")` returns "invalid port
  value", and the validator's DNS-failure fallback is `Ok(())`, so
  the SSRF target slips through.*
- `test_validate_url_dns_rejects_unspecified_addresses`
  — *FIXME: `is_private` does not include `is_unspecified`, so
  `0.0.0.0` and `[::]` route through `to_socket_addrs` and are
  treated as public. Connecting to `0.0.0.0` typically routes to
  localhost on most OSes — SSRF / loopback bypass.*

## Quality gates

- `cargo fmt --check` ✓
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` ✓
- `cargo test --lib -- --test-threads=2` ✓ — 1109 passed, 0 failed,
  2 ignored (the two FIXME-gated tests above)

## SSRF defects identified in production

Both are documented above as `#[ignore]` FIXMEs. Severity / fix
guidance:

1. **Bracketed IPv6 host without port bypasses DNS check** (Medium).
   Fix: in `validate_url_dns`, when `host_port` starts with `[` and
   ends with `]` with no `:` after the closing bracket, append `:80`
   to the resolution target (mirror the IPv4 default-port path). The
   current condition `host_port.contains(':') || host_port.starts_with('[')`
   is wrong for bracketed IPv6 with no explicit port: `[fe80::1]`
   contains a `:` inside the brackets but has no socket port, and
   `ToSocketAddrs` requires the port outside the brackets.
2. **Unspecified addresses (`0.0.0.0`, `[::]`) accepted** (Medium).
   Fix: in `is_private`, add `v4.is_unspecified()` to the v4 arm and
   `v6.is_unspecified()` to the v6 arm. (`Ipv4Addr::is_unspecified`
   covers exactly `0.0.0.0`; `Ipv6Addr::is_unspecified` covers `::`.)
   Both unspecified addresses route to localhost on most OSes — they
   should be rejected like other private ranges.

Per the brief, **production code was NOT modified** by this lane.
Both defects are visible via the `#[ignore]` annotations and a
follow-up fix can simply remove the `#[ignore]` to validate.

## Surprises / deviations

- **Brief expected `Err` for loopback; production allows it.** The
  brief listed `127.0.0.1`, `localhost`, `[::1]` as expected-`Err`
  cases for `validate_url_dns`. The production code intentionally
  allows loopback (see comment at line 347:
  `if is_private(ip) && !ip.is_loopback() { return Err(...) }`) —
  loopback webhooks are a documented dev/CI feature and the layered
  defence is `validate_url`'s scheme gate which forces non-loopback
  hosts onto https. The tests were written to assert the documented
  ALLOW behaviour, with comments explaining the layering. Tightening
  loopback handling here would be a behaviour change, not just a
  test addition, and is out of scope for this lane.
- **Two real SSRF defects surfaced.** The link-local-v6 and
  unspecified-address tests both revealed SSRF gaps and are gated
  `#[ignore]` with FIXME tags per the brief. The fixes are tiny
  (append `:80` for bracketed-no-port IPv6, add `is_unspecified` to
  `is_private`) but were left out of scope per the constraint.
- **Disjoint from L10a.** No edits to `llm.rs`. Coverage gain on
  `subscriptions.rs` is +5.55 pts; codebase-wide +0.88 pts.

## Commits

(See `git log cov-90pct-w10/subscriptions-ssrf ^origin/cov-90pct-w9/consolidated`.)
