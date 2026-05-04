# Closer M9 — W9 mcp.rs sweep

Branch: `cov-90pct-w9/mcp-sweep`
Base: `origin/cov-90pct-w8/consolidated` (`1879e53`)

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --no-fail-fast --json -- --test-threads=2 > /tmp/closer-m9-cov.json 2>&1
```

## Headline numbers

| Surface           | Pre-M9 (W8 base) | Post-M9  | Delta     |
|-------------------|------------------|----------|-----------|
| Codebase line     | 88.15%           | 88.40%   | +0.25 pp  |
| Codebase region   | 88.21%           | 88.56%   | +0.35 pp  |
| Codebase fn       | 86.57%           | 86.85%   | +0.28 pp  |
| mcp.rs line       | 76.22%           | 80.48%   | +4.26 pp  |
| mcp.rs region     | 72.19%           | 78.38%   | +6.19 pp  |
| mcp.rs fn         | 42.21%           | 50.42%   | +8.21 pp  |

**Lines covered (codebase):** 32,608 / 36,885.
**Lines covered (mcp.rs):**   3,092 / 3,842 (+633 vs pre).

mcp.rs picked up ~633 newly-covered lines from the 40 tests in this
batch — landing on the per-handler happy/error pairs, the JSON-RPC
framing branches (parse error / unknown method / wrong version /
notification short-circuit), the `auto_register_path_hierarchy`
early-return + walk paths, and the four `inject_namespace_standard`
shape branches (single object vs `standards` array, dedup against
result memories, no-namespace global-only path).

## Tests added (40)

All appended to `src/mcp.rs::tests`. No production code touched.

### Tool handlers (18)

Per-handler happy + error pairs that drive `handle_request` directly
through `tools/call` envelopes:

- `handle_store_happy_returns_id_and_tier`
- `handle_store_error_missing_title`
- `handle_recall_happy_returns_memories_array`
- `handle_recall_error_budget_tokens_zero`
- `handle_search_happy_returns_results_array`
- `handle_search_error_missing_query`
- `handle_get_happy_returns_memory`
- `handle_get_error_unknown_id`
- `handle_list_happy_returns_memories_array`
- `handle_list_error_invalid_agent_id`
- `handle_delete_happy_removes_existing_memory`
- `handle_delete_error_empty_id`
- `handle_link_happy_returns_linked_true`
- `handle_link_error_missing_target_id`
- `handle_promote_error_unknown_id`
- `handle_consolidate_error_missing_summary_keyword_tier`
- `handle_capabilities_happy_returns_tier_struct`
- `handle_subscribe_error_unregistered_agent`

### JSON-RPC framing (11)

Drives the new `dispatch_line` test-only helper plus `handle_request`
directly to cover parse-time and protocol-level error paths:

- `test_jsonrpc_handles_well_formed_request`
- `test_jsonrpc_handles_malformed_json`
- `test_jsonrpc_handles_truncated_request`
- `test_jsonrpc_handles_two_requests_per_line`
- `test_jsonrpc_handles_blank_line`
- `test_jsonrpc_handles_notification_no_response`
- `test_jsonrpc_handles_method_not_found`
- `test_jsonrpc_handles_invalid_params`
- `test_jsonrpc_handles_unknown_tool_returns_minus_32601`
- `test_jsonrpc_rejects_wrong_version`
- `test_jsonrpc_handles_initialize`

### auto_register_path_hierarchy (5)

- `test_auto_register_creates_top_level_namespace`
- `test_auto_register_creates_nested_hierarchy`
- `test_auto_register_idempotent`
- `test_auto_register_handles_empty_string_or_root`
- `test_auto_register_skips_when_explicit_parent_set`

### inject_namespace_standard (6)

- `test_inject_namespace_standard_attaches_when_present`
- `test_inject_namespace_standard_skips_when_absent`
- `test_inject_namespace_standard_top_of_recall_response`
- `test_inject_namespace_standard_preserves_other_response_fields`
- `test_inject_namespace_standard_no_namespace_uses_global`
- `test_inject_namespace_standard_multiple_levels_emits_array`

## Quality gates

- `cargo fmt --check`: clean
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic`: clean
- `cargo test --lib -- --test-threads=2`: 1070 passed / 0 failed

## Inner-fn factor-out

Yes — added `dispatch_line(&Connection, &str) -> Option<RpcResponse>`
inside `mcp::tests` (NOT in production code). It mirrors the
parse-then-dispatch portion of `run_mcp_server`'s stdin loop for one
line, so framing tests can drive parse-error, blank-line, and
notification-skip branches without spinning up a real stdio server.
The helper is `#[cfg(test)]` and visible only to the tests module —
no public API change.

A second test-only helper, `invoke_handle_request(&Connection,
&RpcRequest)`, wraps the boilerplate around `handle_request` so the
new tool-handler tests don't repeat the 13-arg call site.

## Constraints honored

- Test-only changes; production code untouched.
- All new tests appended at the end of `src/mcp.rs::tests`.
- No new dependencies added.
- federation.rs, autonomy.rs, curator.rs untouched (W9 lane disjoint).
