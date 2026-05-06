# v0.7 E2 — post-ship convergence verification runbook

> **Status:** SHIPPING with v0.7.0 — Track E task E2
> **Owner:** release captain on duty when F5 (release-tag) lands
> **Script:** [`scripts/post-ship-converge.sh`](../../scripts/post-ship-converge.sh)
> **Sister task:** E1 (`scripts/t0-orchestrate.sh`) — same 6-question
> set, run against the in-tree cargo build instead of the published
> artefact.

---

## Why this exists

CI proves "the source tree compiles and the test binary answers the
6 Discovery Gate questions correctly". It does **not** prove "the
crate users `cargo install` from crates.io behaves the same way":

- `[package].include` exclusions (cargo packages a subset of the repo
  tree) can drop a `migrations/` file and break first-run.
- Release-profile codegen baselines (`opt-level`, `target-cpu`) can
  change behaviour vs. the dev-profile binary CI runs.
- Feature-flag defaults can diverge between `cargo test` and the
  no-features-enabled `cargo install` path.
- Brew bottles, GitHub-release tarballs, and the `cargo install` path
  are three independent supply chains; any of them can carry a
  packaging regression that CI cannot catch.

This runbook closes the "passes CI but breaks for users" loop: within
1 hour of F5 landing, install the published binary on a fresh-as-
possible machine and replay the 6 canonical Discovery Gate questions
against it.

---

## When to run

| Trigger | Window | Owner |
|---|---|---|
| F5 release-tag lands (`v0.7.0`, `v0.7.1`, `v0.7.x`) | within 1 hour | release captain on duty |
| Crate yank-and-republish (rare — version bump) | within 1 hour of new tag | release captain on duty |
| Brew tap formula update | within 24 hours | packaging maintainer |

The 1-hour window is calibrated against crates.io propagation:
the index typically converges in <5 min, but the sparse index has
been observed to lag on regional CDNs for ~30 min.

---

## How to run

### Cargo install (default — most cross-platform)

```bash
scripts/post-ship-converge.sh --version 0.7.0
```

This runs `cargo install ai-memory --version 0.7.0` into a temp dir
and replays the 6 canonical questions against the resulting binary.

### Brew tap

```bash
scripts/post-ship-converge.sh --version 0.7.0 --method brew
```

Use this on macOS hosts to verify the bottle. Requires the
`alphaonedev/tap` tap configured.

### GitHub release binary

```bash
scripts/post-ship-converge.sh --version 0.7.0 --method binary
```

Pulls the prebuilt artefact from
`https://github.com/alphaonedev/ai-memory-mcp/releases/download/v0.7.0/`
and verifies it. Use this to validate the release-asset path
specifically (the Homebrew bottle is downstream of this artefact).

### Dry-run (CI smoke)

```bash
scripts/post-ship-converge.sh --version 0.7.0 --dry-run
```

Skips the install + spawn steps and emits the JSON envelope with
`dry_run: true`. Used by `tests/e2_post_ship_dry_run.rs` to keep
the script output structure under CI guard.

---

## How to interpret the verdict

The script prints a structured JSON document on stdout and a
human-readable line on stderr. The top-level `verdict` field is one
of:

| Verdict | Meaning | Action |
|---|---|---|
| `GREEN` | All 6 questions matched canonical phrasing | Stand down. Record the run in the ship-day timeline. |
| `RED` | At least one question drifted | Escalate (see below). |
| `DRY_RUN` | `--dry-run` was passed | No action — for CI smoke only. |

The exit code carries the same signal: `0` for `GREEN`, `2` for `RED`,
`3` for usage errors, `4` for install failures.

Per-question results live in `.results[]`. Each entry carries:

```json
{ "id": "Q1-T0-A2-CORE", "profile": "core", "kind": "exact",
  "status": "PASS" | "FAIL" | "SKIPPED_DRY_RUN", "response": ... }
```

A `FAIL` carries the actual response so the post-mortem can
reconstruct what the published binary said vs. what was expected.

---

## The 6 canonical questions

(Sister to E1's `scripts/t0-orchestrate.sh`. Drift between the two
question sets is itself a bug — both must change in lockstep.)

| ID | Profile | Field | Kind | Source cell |
|---|---|---|---|---|
| Q1-T0-A2-CORE | core | `to_describe_to_user` | exact match | [`tests/calibration_t0.rs::t0_describe_to_user_core_profile_canonical_phrasing`](../../tests/calibration_t0.rs) |
| Q2-T0-A2-GRAPH | graph | `to_describe_to_user` | exact match | [`tests/calibration_t0.rs::t0_describe_to_user_graph_profile_canonical_phrasing`](../../tests/calibration_t0.rs) |
| Q3-T0-A2-FULL | full | `to_describe_to_user` | exact match | [`tests/calibration_t0.rs::t0_describe_to_user_full_profile_canonical_phrasing`](../../tests/calibration_t0.rs) |
| Q4-T0-A1-CORE-RECOVERY-PATHS | core | `summary` | contains 4 paths | [`tests/calibration_t0.rs::t0_summary_core_profile_lists_four_recovery_paths`](../../tests/calibration_t0.rs) |
| Q5-T0-NO-JARGON-FULL | full | `to_describe_to_user` | absent of MCP jargon | [`tests/calibration_t0.rs::t0_describe_to_user_omits_mcp_jargon_across_profiles`](../../tests/calibration_t0.rs) |
| Q6-T0-CONTRACT-CORE | core | (envelope) | schema-shape | [`tests/calibration_t0.rs::t0_v3_contract_both_strings_present_under_every_named_profile`](../../tests/calibration_t0.rs) |

---

## Escalation path on RED verdict

A RED verdict means the published binary disagrees with the canonical
phrasing pinned in CI. This is a release-blocker; treat it like a
P0 production incident.

1. **Stop the ship-day announcement loop.**
   Do not push the release announcement to the website / changelog
   feed / social channels. If it has already gone out, post a hold
   notice referencing this runbook.

2. **Yank the crate (if cargo-method failed).**
   ```
   cargo yank --version 0.7.0
   ```
   This stops `cargo install ai-memory` from picking up the broken
   point release. Existing `Cargo.lock` files with `0.7.0` already
   pinned will keep building (yank does not delete) — that is
   intentional; yank only blocks new resolutions.

3. **Pull the brew bottle (if brew-method failed).**
   `brew tap-pin alphaonedev/tap` then revert the tap formula PR.

4. **Pull the GitHub release asset (if binary-method failed).**
   Mark the GitHub release as "Pre-release" and edit the body with
   a banner pointing at the post-mortem issue.

5. **Open the post-mortem issue.**
   Title: `post-ship convergence RED on v0.7.X — <symptom>`.
   Required body sections:
   - Verdict JSON from this script (full `.results[]` array).
   - Install method that surfaced the failure.
   - Diff between the canonical phrasing and what the published
     binary returned.
   - Hypothesis for which packaging step introduced the drift
     (e.g., "`migrations/v18.sql` excluded by `[package].include`").
   - Repro steps on a fresh machine.

6. **File a follow-up release.**
   `v0.7.X.1` (or `v0.7.X+1` per semver discretion) with the
   packaging fix. Re-run this runbook against the new tag before
   un-yanking — do not un-yank a known-broken version.

7. **Update CI to catch the regression next time.**
   The point of E2 is to surface what CI missed. Every RED verdict
   must end with either a new test case in `tests/` or a new
   packaging assertion in CI (e.g., `cargo package --list | grep …`)
   so the same drift cannot recur silently.

---

## Refs

- [v0.7.0 epic](./V0.7-EPIC.md) — track E, tasks E1–E3
- [Canonical phrasings](./canonical-phrasings.md) — the source of truth
  the published binary is being compared against
- [`tests/calibration_t0.rs`](../../tests/calibration_t0.rs) — the
  in-tree T0 cells this script mirrors
- [`tests/e2_post_ship_dry_run.rs`](../../tests/e2_post_ship_dry_run.rs)
  — CI guard on the script's output envelope
