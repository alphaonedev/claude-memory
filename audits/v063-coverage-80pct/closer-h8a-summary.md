# Closer H8a — handlers.rs archive lane sweep

Branch: `cov-90pct-w8/handlers-archive` (pushed: yes)
Base: `origin/cov-90pct-w7/integration-tests` (eafaf84)

## Tests added: 28

Per-handler distribution:

| handler            | new tests |
|--------------------|-----------|
| `list_archive`     | 5         |
| `archive_by_ids`   | 5         |
| `purge_archive`    | 4         |
| `restore_archive`  | 5         |
| `archive_stats`    | 3         |
| `forget_memories`  | 6         |
| **total**          | **28**    |

All tests appended at the end of the existing
`#[cfg(test)] mod tests` block in `src/handlers.rs` so cherry-pick
conflicts with H8b/c/d are avoided.

Reused existing helpers without adding new ones:

- `test_state()` — fresh `:memory:` Db state
- `test_app_state(db)` — wraps Db into AppState (no embedder, no fed)
- `insert_test_memory(state, ns, title)` — seed an active row
- `Router::new() / .oneshot(...)` — full HTTP roundtrip

## Coverage

| metric                       | before W8/H8a | after W8/H8a | delta     |
|------------------------------|---------------|--------------|-----------|
| combined (overall lines)     | 85.85%        | 86.23%       | +0.38 pp  |
| `src/handlers.rs` lines      | 81.09%        | 83.02%       | +1.93 pp  |
| `src/handlers.rs` regions    | n/a           | 86.82%       | —         |
| `src/handlers.rs` functions  | n/a           | 92.76%       | —         |

## Quality gates

- `cargo fmt --check` ✓
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` ✓
- `cargo test --lib -- --test-threads=2` ✓ (944 passed, was 916)

## What each batch covers

### `list_archive` (5)
- empty DB → `{archived:[], count:0}` 200
- two archived rows → `count:2`
- pagination: 3 rows / `limit=1&offset=1` → `count:1`
- namespace filter excludes the other namespace's row
- unknown namespace → `count:0` (200, not 404)

### `archive_by_ids` (5)
- single live id → `count:1`, `missing:[]`, default `reason="archive"`
- bulk (3 live ids) → all archived, none missing
- `ids:[]` → 200 with `count:0`, archive untouched
- missing `ids` field → 4xx (Json extractor rejects)
- malformed JSON body → 4xx

### `purge_archive` (4)
- `older_than_days=365` keeps recent rows (`purged:0`)
- no query → purges all (`purged:3`, archive empty after)
- `older_than_days=0` purges everything older than "now"
- response shape always has numeric `purged` key

### `restore_archive` (5)
- happy path: `restored:true`, row in `memories`, gone from archive
- list_archive after restore → `count:0`
- restore preserves namespace + title
- restore-after-purge → 404 (archive row gone)
- oversized id (>128 bytes) → 400 from `validate_id`

### `archive_stats` (3)
- with data: `archived_total:3` + DESC-ordered `by_namespace`
- empty DB: `archived_total:0`, empty breakdown
- active-only DB: archive stats unaffected (`archived_total:0`)

### `forget_memories` (6)
- no filter → 400 (db::forget bails)
- pattern only — only matching rows deleted (`deleted:2`)
- by tier — only that tier's rows deleted (`deleted:2`)
- combined namespace + pattern intersect (AND, not OR)
- malformed JSON → 4xx
- no-match filter → 200 `deleted:0`, other rows untouched

## Surprises / deviations

None major. One drafted test (`http_restore_archive_invalid_id_charset_returns_400`)
relied on `<>` failing `validate_id`, but `is_clean_string` only rejects
control characters, not `<`/`>`. Replaced with an oversized-id (200-byte)
test that exercises the same 400 arm via the `MAX_ID_LEN` guard.

## Commits

- `8c35c87` — test(handlers): W8/H8a — archive lane sweep (~28 tests across 6 handlers)
