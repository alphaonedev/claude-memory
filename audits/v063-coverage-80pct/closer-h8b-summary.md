# Closer H8b — Wave 8 inbox/subscriptions handler coverage

**Branch:** `cov-90pct-w8/handlers-inbox`
**Lane:** handlers.rs inbox/subscriptions (subscribe / unsubscribe /
list_subscriptions / notify / get_inbox / session_start)
**Base:** `origin/cov-90pct-w7/integration-tests`

## Coverage delta

| Scope                 | Pre (W7) | Post (H8b) | Δ pp     |
| --------------------- | -------- | ---------- | -------- |
| Combined (lines)      | 85.85 %  | 86.33 %    | +0.48 pp |
| `src/handlers.rs`     | 81.09 %  | 83.45 %    | +2.36 pp |
| `handlers.rs` regions | n/a      | 86.84 %    | —        |
| `handlers.rs` fns     | n/a      | 92.92 %    | —        |

Detailed JSON snapshot is committed alongside this summary at
`audits/v063-coverage-80pct/closer-h8b-coverage.json`.

## Tests added (27 — appended to `handlers.rs::tests`)

All names prefixed `h8b_` for traceability.

| Handler              | Count | Names |
| -------------------- | ----- | ----- |
| `subscribe`          | 7     | `https_url_returns_created`, `missing_url_and_namespace_rejected`, `invalid_url_rejected`, `rejects_link_local_metadata_ip`, `namespace_shape_synthesizes_url`, `event_filter_round_trips`, `persists_hmac_secret` |
| `unsubscribe`        | 4     | `by_id_happy_path`, `nonexistent_id_returns_removed_false`, `by_agent_and_namespace`, `missing_id_and_namespace_rejected` |
| `list_subscriptions` | 2     | `returns_seeded_rows`, `agent_id_filter_excludes_others` |
| `notify`             | 5     | `happy_path_creates_message`, `missing_target_agent_id_rejected`, `invalid_target_agent_id_rejected`, `oversized_payload_rejected`, `accepts_content_alias_for_payload` |
| `get_inbox`          | 5     | `empty_returns_zero`, `returns_pending_after_notify`, `unread_only_filter_excludes_read`, `limit_clamps_returned_count`, `invalid_agent_id_rejected` |
| `session_start`      | 4     | `with_valid_agent_id_echoes`, `namespace_filter`, `returns_session_id_without_agent`, `preloads_recent_context` |

(`list_subscriptions` lane only added 2 net-new — the W2/W3 baseline already
shipped `http_list_subscriptions_empty_returns_zero` and the agent-id
filter case; H8b strengthens those with the seeded-rows shape assertion
and the explicit agent-vs-other exclusion test.)

## Quality gates

| Gate                                   | Result |
| -------------------------------------- | ------ |
| `cargo fmt --check`                    | pass   |
| `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` | pass |
| `cargo test --lib -- --test-threads=2` | 943 pass, 0 fail |

## Surprises / deviations

One assertion needed loosening after the first run: in
`h8b_get_inbox_returns_pending_after_notify` the `from` field on a
delivered message is whatever
`identity::resolve_agent_id(None, mcp_client)` returns — and that helper
synthesises `ai:<client>@<host>:pid-N` rather than echoing the bare
caller id when only `mcp_client` is set. The test now accepts either
the bare form (`alice`) or the synthesised long form
(`ai:alice@…`). No production-code change.

The HMAC subscription tests are scoped to the inbound handler only — they
verify that `secret` is accepted, persisted in the `subscriptions` row,
and not echoed in the response. Outbound dispatcher signing/verification
lives in `subscriptions.rs` and is owned by a different lane.

## Commits

- `e17d712` test(handlers): W8/H8b — inbox/subscriptions handler coverage
