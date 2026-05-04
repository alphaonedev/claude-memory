# Closer L10a — Wave 10 (llm.rs HTTP mock pass)

**Branch:** `cov-90pct-w10/llm-mocks`
**Date:** 2026-04-26
**Owner:** Closer L10a
**Files:** `src/llm.rs` (test-only append), `Cargo.toml` (added `wiremock = "0.6"` to `[dev-dependencies]`)

## Coverage delta

| File          | Pre (W7 baseline brief) | Post (W10 lines) | Δ      |
|---------------|------------------------:|-----------------:|-------:|
| llm.rs        | 65.78%                  | **94.80%**       | +29.02 |
| Codebase      | ~85.0% (post-W9)        | **85.71%**       | +~0.7  |

llm.rs lines: `1166 / 1230` covered. Functions: 95.74%. Regions: 94.42%.

## Tests added (16, all `wiremock`-driven)

A new `mod wiremock_tests` block was appended to `src/llm.rs` (existing
`mod tests` and `mod mock_tests` left untouched per the APPEND
directive). Every test boots an in-process `wiremock::MockServer`
speaking the actual Ollama API surface (`/api/tags`, `/api/chat`,
`/api/embed`, `/api/pull`) and drives `OllamaClient` end-to-end through
its real blocking-`reqwest` call paths — no real Ollama daemon, no
network egress.

### Per-area breakdown

- **`is_available` (3):**
  1. `test_is_available_returns_false_on_connection_refused` — port
     reservation pattern: bind a `TcpListener` to `127.0.0.1:0`, capture
     the port, drop the listener, point reqwest at the now-free port.
  2. `test_is_available_returns_false_on_500_response` — also exercises
     the `new_with_url` "not running or not reachable" error branch.
  3. `test_is_available_returns_true_on_200_with_json_body`.

- **`ensure_model` / pull-if-missing (2):**
  4. `test_pull_if_missing_skips_pull_if_model_already_in_tags` —
     `/api/tags` returns the model; mounted `/api/pull` is `expect(0)`.
  5. `test_pull_if_missing_initiates_pull_if_not` — `/api/tags` empty;
     `/api/pull` is `expect(1)` with `body_partial_json({"name":...})`.

- **`generate` (4):**
  6. `test_generate_parses_success_response` — `{"message":{"content":"hello"}}`
     parsed verbatim.
  7. `test_generate_returns_error_on_malformed_json` — body is
     `{not valid json` with `content-type: application/json`; asserts
     parse error surfaced.
  8. `test_generate_returns_error_on_500`.
  9. `test_generate_passes_system_prompt_when_provided` — covers the
     `if let Some(sys) = system` branch via `body_partial_json` matcher
     on `messages: [{role:"system",...},{role:"user",...}]`.

- **`embed_text` (3):**
  10. `test_embed_parses_embedding_array` — Ollama `/api/embed` returns
      `{"embeddings": [[0.1, 0.2, 0.3]]}`; asserts `Vec<f32>` shape and
      values within 1e-5.
  11. `test_embed_returns_error_on_wrong_shape` — `{"embedding": 0.5}`
      (singular scalar) → "Missing embeddings" error.
  12. `test_embed_returns_error_on_500`.

- **`ensure_embed_model` (1):**
  13. `test_ensure_embed_model_skips_pull_if_present`.

- **higher-level helpers (3):**
  14. `test_expand_query_returns_parsed_terms_one_per_line` — chat
      returns `"term1\nterm2\nterm3\n\n"`; asserts
      `vec!["term1","term2","term3"]` (blank line filtered).
  15. `test_auto_tag_returns_parsed_tags` — chat returns
      mixed-case `"Tag1\nTAG2\ntag3"`; asserts module lowercases each
      line to `["tag1","tag2","tag3"]`.
  16. `test_detect_contradiction_parses_yes_no` — three sub-cases in
      one test (yes → true, no → false, garbage → false), each backed
      by its own `MockServer` so the dispatch is deterministic.

## Quality gates

- `cargo fmt --check` ✓
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic` ✓
- `cargo test --lib -- --test-threads=2` ✓ — **1119 passed**
  (was 1103 post-W9; +16 net tests, exactly the wiremock additions).
- llvm-cov JSON: see `closer-l10a-coverage.json`.

## Surprises / deviations

- **Endpoint path mismatch in the brief.** The brief refers to
  `/api/generate` and `/api/embeddings`; the actual `OllamaClient`
  hits `/api/chat` and `/api/embed` with response shapes
  `{"message":{"content":"..."}}` and `{"embeddings":[[...]]}`
  respectively. Tests align to the actual code, not the brief.
- **`OllamaClient` is `reqwest::blocking`, wiremock is async.** Each
  test uses `#[tokio::test(flavor = "multi_thread")]` and runs the
  blocking client through `tokio::task::spawn_blocking` so the
  blocking calls don't deadlock the runtime hosting the mock server.
- **`new_with_url` does an embedded health probe.** Every test that
  needs to construct a real client first mounts a permissive
  `/api/tags → 200 {"models":[]}` route via the `mount_tags_ok` helper
  so the constructor's `is_available()` check passes; tests that want
  to drive specific tag behaviour mount their precise responder ahead.
- **`OllamaClient` does not implement `Debug`.** That precludes the
  usual `result.unwrap_err()` shortcut on a `Result<OllamaClient, _>`
  — the 500-response test pattern-matches the `Err` arm directly to
  pull the message out instead.
- **Connection-refused test has a small race window** between
  `drop(listener)` and the reqwest probe. The `is_available()` call
  uses a 5s timeout, so the worst-case flake is a slow test rather
  than a wrong assertion. The pre-existing
  `MockOllamaClient::with_failure(NetworkError, ..)` path is the
  belt-and-braces fallback; this new test exercises the real reqwest
  error branch.
- **Existing `MockOllamaClient` left in place.** The pre-W10 baseline
  already had 28 mock-based tests against a hand-rolled in-tree mock
  client. Those test the *mock*, not the production HTTP surface; they
  are kept because they cheaply cover the higher-level Result wiring
  (timeouts, malformed-response variants, etc.) and aren't redundant
  with the new wiremock-driven coverage of the actual reqwest paths.
