# v0.7.0 L0.7 Gap Closures

Source: L0.7-1 baseline at HEAD bfe9650.

## Headline

- **Total line coverage: 85.66%** (target after L0.7: 95-96%)
- **Function coverage: 80.55%**
- 138 modules, 68 below tier target, 70 at/above

## L0.7-2..6 priority work (largest gaps first)

| Module | Tier | Target | Current | Gap | L0.7 sub-task |
|--------|------|--------|---------|-----|---------------|
| ~~transcripts/replay.rs~~ | ~~A~~ → **F** | — | 0.0% | — | **deferred to L2-4** (L0.5.5-3 placeholder; reclassified 2026-05-13, see bugs_surfaced 1a2a5a8a) |
| mcp/tools/reflect.rs | B | 95% | 0.0% | 95.0% | L0.7-3 |
| store/postgres.rs | E | 90% | 13.9% | 76.1% | L0.7-6 |
| mcp/tools/auto_tag.rs | D | 85% | 18.4% | 66.6% | L0.7-5 |
| mcp/tools/detect_contradiction.rs | D | 85% | 22.6% | 62.4% | L0.7-5 |
| handlers/federation_receive.rs | B | 95% | 42.5% | 52.5% | L0.7-3 |
| handlers/http.rs | B | 95% | 43.7% | 51.3% | L0.7-3 |
| handlers/hook_subscribers.rs | B | 95% | 46.7% | 48.3% | L0.7-3 |
| store/mod.rs | E | 90% | 42.2% | 47.8% | L0.7-6 |
| mcp/tools/check_duplicate.rs | B | 95% | 48.1% | 46.9% | L0.7-3 |
| mcp/tools/store.rs | B | 95% | 54.8% | 40.2% | L0.7-3 |
| cli/schema_init.rs | B | 95% | 56.9% | 38.1% | L0.7-3 |
| store/sqlite.rs | E | 90% | 57.5% | 32.5% | L0.7-6 |
| mcp/tools/consolidate.rs | B | 95% | 62.8% | 32.2% | L0.7-3 |
| mcp/tools/recall.rs | B | 95% | 63.0% | 32.0% | L0.7-3 |
| handlers/transport.rs | E | 90% | 61.9% | 28.1% | L0.7-6 |
| mcp/tools/expand_query.rs | D | 85% | 60.0% | 25.0% | L0.7-5 |
| mcp/tools/entity_get_by_alias.rs | B | 95% | 71.0% | 24.0% | L0.7-3 |
| hooks/recall.rs | C | 92% | 69.9% | 22.1% | L0.7-4 |
| mcp/tools/session_start.rs | B | 95% | 74.5% | 20.5% | L0.7-3 |
| mcp/tools/update.rs | B | 95% | 74.8% | 20.2% | L0.7-3 |
| mcp/tools/delete.rs | B | 95% | 75.9% | 19.1% | L0.7-3 |
| federation/receive.rs | C | 92% | 73.9% | 18.1% | L0.7-4 |
| models/memory.rs | A | 98% | 80.8% | 17.2% | L0.7-2 |
| mcp/tools/archive.rs | B | 95% | 78.2% | 16.8% | L0.7-3 |
| cli/recall.rs | B | 95% | 79.4% | 15.6% | L0.7-3 |
| mcp/tools/find_paths.rs | B | 95% | 79.5% | 15.5% | L0.7-3 |
| mcp/tools/namespace.rs | B | 95% | 80.1% | 14.9% | L0.7-3 |
| mcp/tools/link.rs | B | 95% | 80.2% | 14.8% | L0.7-3 |
| cli/governance_migrate.rs | B | 95% | 80.2% | 14.8% | L0.7-3 |
| mcp/tools/replay.rs | B | 95% | 80.4% | 14.6% | L0.7-3 |
| hooks/executor.rs | C | 92% | 78.0% | 14.0% | L0.7-4 |
| cli/shell.rs | B | 95% | 81.7% | 13.3% | L0.7-3 |
| mcp/tools/promote.rs | B | 95% | 81.8% | 13.2% | L0.7-3 |
| quotas.rs | ? | 90% | 76.9% | 13.1% | ? |
| mcp/tools/pending.rs | B | 95% | 82.5% | 12.5% | L0.7-3 |
| storage/migrations.rs | A | 98% | 86.6% | 11.4% | L0.7-2 |
| mcp/tools/subscribe.rs | B | 95% | 84.9% | 10.1% | L0.7-3 |
| mcp/tools/load_family.rs | B | 95% | 84.9% | 10.1% | L0.7-3 |
| log_paths.rs | A | 98% | 88.3% | 9.7% | L0.7-2 |
| _...38 more lower-priority gaps_ | | | | | |

## Tier rollup

| Tier | Files | Avg current | Below target |
|------|-------|-------------|--------------|
| ? | 5 | 94.0% | 1 |
| A | 26 | 92.4% | 14 |
| B | 71 | 85.1% | 47 |
| C | 18 | 89.7% | 8 |
| D | 4 | 48.5% | 3 |
| E | 11 | 76.6% | 5 |
| F | 3 | 63.2% | 0 |

## Discipline

Per L0.7 playbook (memory 17148c61):
- **Thresholds RISE, never FALL.** L0.7-7 will set CI thresholds at CURRENT levels with "raise to <tier-target> by v0.8.0" comments.
- **No #[ignore], no test::skip.** Flakiness gets fixed.
- **3-consecutive-run flakiness check on every test addition.**
- **Visibility-only production changes acceptable** (pub → pub(crate) for testability). Any semantic change → STOP, surface bug.
- **LLM call sites: stub deterministically.** Real Gemma 4 inference NEVER in cargo test.
