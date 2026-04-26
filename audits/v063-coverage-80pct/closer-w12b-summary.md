# Closer W12-B — handlers.rs long-tail sweep

Branch: `cov-90pct-w12/handlers-longtail` (pushed: pending)
Base: `origin/cov-90pct-w11/consolidated` (9eeb453)

## Tests added: 98

Two passes:

1. First pass (~85 tests) — broad long-tail coverage sweep.
2. Follow-up (~13 tests) — governance Pending + remaining edge arms.

| group                                          | new tests |
|------------------------------------------------|-----------|
| `percent_decode_lossy` / `constant_time_eq`    | 11        |
| `api_key_auth` middleware extra arms           | 4         |
| health / prometheus_metrics / list_namespaces  | 3         |
| `get_taxonomy` variants                        | 3         |
| `get_memory` / `update_memory` / `delete_memory` / `promote_memory` | 12 |
| `create_link` / `delete_link` / `get_links`    | 5         |
| `get_stats` / `run_gc` / `export` / `import`   | 4         |
| recall + search invalid `as_agent`             | 4         |
| `forget_memories` / `archive_*`                | 5         |
| kg_query happy / kg_invalidate happy / kg_timeline since/until | 3 |
| `notify` / `subscribe` / `unsubscribe` / `list_subscriptions` | 7 |
| `session_start` / `entity_register` / `entity_get_by_alias`  | 4 |
| `sync_push` oversize sweep + invalid agent ids | 7         |
| `consolidate_memories` / `bulk_create`         | 2         |
| approve/reject pending invalid header         | 2         |
| create_memory invalid x-agent-id / scope      | 2         |
| list_memories invalid agent_id filter         | 1         |
| check_duplicate blank namespace               | 1         |
| Governance Pending (create/delete/promote)    | 5         |
| Misc edge arms                                | 13        |
| **total**                                     | **98**    |

All tests appended at the end of the existing `#[cfg(test)] mod tests`
block in `src/handlers.rs`. No production-code changes. Reused existing
helpers (`test_state`, `test_app_state`, `insert_test_memory`,
`auth_app`) plus a new local helper `seed_governance_policy(state, ns,
policy_json)` that inserts a `_namespace_standard` row and wires
`namespace_meta` to it.

## Coverage

| metric                       | before W12-B | after W12-B | delta     |
|------------------------------|--------------|-------------|-----------|
| `src/handlers.rs` lines      | 88.43%       | 92.69%      | +4.26 pp  |
| `src/handlers.rs` regions    | n/a          | 94.43%      | —         |
| `src/handlers.rs` functions  | n/a          | 96.66%      | —         |
| codebase lines (sal feature) | 85.82%       | 87.42%      | +1.60 pp  |

Target was 93%+ on handlers.rs; reached 92.69%. The remaining ~1000
uncovered lines on handlers.rs are mostly federation-fanout match arms
(`broadcast_*_quorum` Ok / Err / `finalise_quorum` 503 paths) that are
already covered for `set_namespace_standard_qs` via the H8d mock-peer
fixture but would require similar fixtures for `register_agent`,
`approve_pending`, `reject_pending`, `delete_memory`, `promote_memory`,
`create_link`, `consolidate_memories`, etc. Those are the natural target
for a future W13 closer.

## Quality gates

- `cargo fmt --check` ✓
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` ✓
- `cargo test --lib -- --test-threads=2` ✓ (1245 passed, was 1147)

## Surprises / deviations

- Several drafted tests assumed `validate_id` / `validate_namespace`
  reject characters they actually allow. `validate_id` only rejects
  empty / >128-byte / control-char inputs (URL-special chars like `!@#`
  pass). `validate_namespace` allows newlines (only spaces, `\\`, `\0`,
  control chars are rejected) and the trim happens before the
  empty-segment check. Fixed by switching to oversized-id (200-byte)
  and `foo//bar` empty-segment inputs.
- The `sync_push.pendings` shape took two iterations: the
  `PendingAction` struct uses `action_type`, `requested_by`,
  `requested_at` (not `kind`, `agent_id`, `created_at`) — earlier draft
  caused 422 from the JSON extractor.
- One drafted test for `http_contradictions_requires_topic_or_namespace`
  collided with an existing W8 test of the same name — renamed the new
  one out to maintain append-only semantics.

## Commits

- `47bdb0c` test(handlers): W12-B — long-tail sweep (~85 tests)
- `05dc75d` test(handlers): W12-B follow-up — governance pending + edge arms (~13 tests)
