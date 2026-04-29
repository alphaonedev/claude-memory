# Closer W12-A — W12 mcp.rs deeper sweep

Branch: `cov-90pct-w12/mcp-deeper`
Base: `origin/cov-90pct-w11/consolidated` (`9eeb453`)

## Command (verbatim)

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --features sal --no-fail-fast --json -- --test-threads=2 \
  > /tmp/closer-w12a-cov.json 2>&1
```

## Headline numbers

| Surface           | Pre-W12-A (W11 base) | Post-W12-A | Delta     |
|-------------------|----------------------|------------|-----------|
| mcp.rs line       | 80.48%               | 91.22%     | +10.74 pp |
| mcp.rs region     | 78.38%               | 90.50%     | +12.12 pp |
| mcp.rs fn         | 50.42%               | 72.80%     | +22.38 pp |
| Codebase line     | 88.40%               | 90.92%     | +2.52 pp  |
| Codebase region   | 88.56%               | 91.23%     | +2.67 pp  |
| Codebase fn       | 86.85%               | 90.26%     | +3.41 pp  |

**Lines covered (mcp.rs):** 5,269 / 5,776 (+1,329 vs pre, including added test lines).
**Lines covered (codebase):** 37,298 / 41,021.

mcp.rs cleared the 90% bar on both line and region metrics, going from
80.48% → 91.22% lines (+10.74 pp). Function coverage moved 50.42% →
72.80%; the residual unhit functions are LLM/embedder-only paths
(handle_check_duplicate, handle_auto_tag, handle_detect_contradiction,
handle_expand_query, the recall hybrid+rerank arm, post-store autonomy
hooks) plus the `run_mcp_server` stdio loop, all of which require live
external services that the unit-test harness can't supply.

## Tests added (120)

All appended to `src/mcp.rs::tests`. No production code touched.

### Less-common tool handlers (~40)

- archive: `_list_returns_empty_when_no_archived`,
  `_list_with_namespace_filter`, `_list_with_pagination`,
  `_restore_unknown_id_returns_error`, `_purge_with_older_than_zero`,
  `_purge_no_filter_purges_all`, `_stats_returns_struct`
- kg: `_timeline_unknown_source_returns_empty_events`,
  `_timeline_with_since_until_filters`,
  `_timeline_invalid_since_returns_error`,
  `_timeline_with_seeded_link_returns_event`,
  `_invalidate_no_match_returns_found_false`,
  `_invalidate_with_explicit_valid_until`,
  `_invalidate_invalid_valid_until_format`,
  `_query_with_max_depth_and_filters`, `_query_invalid_valid_at`,
  `_query_rejects_invalid_agent_id`,
  `_query_with_seeded_link_returns_node`
- session/inbox/notify: `handle_session_start_happy_returns_memories`,
  `_session_start_empty_namespace_returns_zero`,
  `_session_start_toon_format_default`,
  `_inbox_returns_empty_for_unregistered_caller`,
  `_inbox_with_unread_only_filter`, `_inbox_with_message_seeded`,
  `_notify_happy_returns_message_id`,
  `_notify_invalid_tier_returns_error`
- subscriptions/agents:
  `_subscribe_with_registered_agent_succeeds`,
  `_subscribe_invalid_url_after_registered`,
  `_unsubscribe_unknown_returns_false`,
  `_unsubscribe_after_subscribe_removes_row`,
  `_list_subscriptions_returns_array`,
  `_list_subscriptions_after_subscribe_returns_one`,
  `_agent_register_then_list`,
  `_agent_register_invalid_type_rejects`
- pending: `_pending_list_happy_returns_array`,
  `_pending_list_with_status_filter`,
  `_pending_approve_unknown_id_returns_error`,
  `_pending_approve_with_seeded_pending_action`,
  `_pending_reject_unknown_id_returns_not_found`,
  `_pending_reject_with_seeded_pending_action`
- gc/forget: `_gc_dry_run_returns_count_without_deleting`,
  `_gc_actual_run_returns_zero_on_empty_db`,
  `_forget_dry_run_with_filters`, `_forget_actual_with_namespace`

### Per-handler error / boundary branches (~30)

- `handle_namespace_set_get_clear_round_trip`,
  `_namespace_get_standard_missing_returns_null`,
  `_namespace_get_standard_inherit_returns_chain`,
  `_namespace_set_standard_with_invalid_governance_rejected`,
  `_namespace_set_standard_invalid_namespace_rejected`,
  `_namespace_set_standard_with_valid_governance`,
  `_namespace_set_standard_with_parent`
- `_entity_register_happy`, `_entity_register_invalid_namespace`,
  `_entity_register_with_explicit_agent_id`,
  `_entity_register_invalid_explicit_agent_id`,
  `_entity_get_by_alias_not_found_returns_null`,
  `_entity_get_by_alias_no_namespace`
- `_get_taxonomy_with_prefix_and_depth`,
  `_get_taxonomy_strips_trailing_slash`,
  `_get_taxonomy_invalid_prefix_after_strip`,
  `_get_taxonomy_invalid_depth_clamps_to_max`
- `_check_duplicate_no_embedder_errors`,
  `_check_duplicate_invalid_title_rejected`,
  `_check_duplicate_invalid_namespace_rejected`
- `_expand_query_no_llm_errors`, `_auto_tag_no_llm_errors`,
  `_detect_contradiction_no_llm_errors`
- `_update_unknown_id_returns_not_found`,
  `_update_invalid_priority_rejected`,
  `_update_with_metadata_object_accepted`,
  `_update_clears_expires_with_empty_string`,
  `_update_change_namespace`
- `_get_links_unknown_id_returns_empty`,
  `_get_links_returns_outbound_and_inbound`
- `_link_invalid_relation_rejected`,
  `_link_creates_link_between_existing_memories`
- `_promote_to_namespace_with_explicit_target`,
  `_promote_invalid_to_namespace_rejected`,
  `_promote_default_tier_to_long`
- `_consolidate_with_explicit_summary_no_llm`,
  `_consolidate_non_string_id_rejected`,
  `_consolidate_succeeds_when_source_was_standard`
- `_get_resolves_by_prefix_and_includes_links`,
  `_delete_with_prefix_id_lookup`
- `_store_dedup_updates_existing`

### TOON format / recall scoring branches (~8)

- `_search_explicit_toon_format`,
  `_recall_explicit_toon_format`,
  `_list_explicit_toon_compact_format`,
  `_search_with_namespace_and_tier_filters`,
  `_search_invalid_agent_id_rejected`,
  `_search_invalid_as_agent_rejected`,
  `_recall_invalid_as_agent_rejected`,
  `_recall_with_context_tokens`,
  `_recall_with_budget_tokens_positive`,
  `_recall_invalid_namespace_filter_passes_through`,
  `_list_with_tier_filter`,
  `_list_invalid_tier_treated_as_none`

### JSON-RPC framing edge cases beyond M9's six (~12)

- `_jsonrpc_handles_ping`,
  `_jsonrpc_handles_notifications_initialized`,
  `_jsonrpc_prompts_list_returns_array`,
  `_jsonrpc_prompts_get_known_name_returns_messages`,
  `_jsonrpc_prompts_get_with_namespace_arg_includes_hint`,
  `_jsonrpc_prompts_get_unknown_name_returns_error`,
  `_jsonrpc_prompts_get_missing_name_returns_error`,
  `_jsonrpc_prompts_get_memory_workflow`,
  `_jsonrpc_tools_call_empty_tool_name_rejected`,
  `_jsonrpc_tools_call_arguments_not_object_uses_empty`,
  `_jsonrpc_tools_call_unicode_in_args`,
  `_jsonrpc_dispatch_line_with_id_zero_treated_as_request`,
  `_jsonrpc_dispatch_line_string_id_passes_through`

### Helper-fn coverage (~13)

- `build_namespace_chain` shape branches:
  `_global_only`, `_simple_namespace`, `_nested_yields_ancestors`,
  `_with_explicit_parent`
- `extract_governance` branches:
  `_default_when_metadata_absent`,
  `_default_when_metadata_invalid`
- `messages_namespace_for`: `_plain_id`, `_ai_prefixed_id`
- `inject_namespace_standard` extras:
  `_no_namespace_no_global`,
  `_dedup_keeps_originals_order`

## Quality gates

- `cargo fmt --check`: clean
- `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::all -D clippy::pedantic`: clean
- `cargo test --lib -- --test-threads=2`: 1225 passed / 0 failed
- `cargo llvm-cov --features sal --no-fail-fast --json -- --test-threads=2`: 1313 tests passed (lib + tests + bins)

## Inner-fn factor-out

None — the `dispatch_line` and `invoke_handle_request` test-only
helpers added by M9 in W9 were sufficient for all 120 new tests.

## Constraints honored

- Test-only changes; production code untouched.
- All new tests appended at the end of `src/mcp.rs::tests`.
- No new dependencies added.
- No other module touched (W12 lane disjoint from the 7 parallel closers).

## Residual unhit branches

The remaining ~9% of mcp.rs lines that stayed unhit cluster around:

1. **LLM/embedder-only paths** (~250 lines): `handle_check_duplicate`'s
   embed/scan loop, `handle_auto_tag`/`handle_detect_contradiction`'s
   LLM call, `handle_expand_query`'s LLM call, `handle_recall`'s
   hybrid+rerank arm and the embedder-fail fallback warn, post-store
   autonomy hooks, the `consolidate` LLM-summary branch.
2. **`run_mcp_server` stdio loop** (~80 lines): tier-config init,
   embedder backfill, HNSW index build, JSON-RPC line loop. Reachable
   only via integration tests that spawn the binary on stdio.
3. **Defensive fallbacks** (~50 lines): tracing::warn paths in store /
   update / consolidate when embed fails; auto-register-path-hierarchy
   when home_dir() returns None.

These would require either live Ollama/embedder fixtures or stdio
integration tests, both out of scope for a `--lib` test-only sweep.
