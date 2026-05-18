// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 D4 — End-to-end integration tests that exercise the
//! harness-detection path (B4) for each known harness.
//!
//! Track B4 reads the MCP `clientInfo.name` field captured at the
//! JSON-RPC `initialize` handshake and maps it to a [`Harness`] enum.
//! The detected harness is then threaded into
//! [`handle_capabilities_with_conn_v3`] so the v3 capabilities response
//! can shape the `your_harness_supports_deferred_registration` boolean
//! based on whether the harness exposes deferred-tool registration.
//!
//! These tests reproduce that path **end-to-end**, in-process, without
//! spawning a subprocess:
//!
//! 1. Synthesize an `initialize` request with `clientInfo.name` set to
//!    a known harness name (`claude-code`, `cursor`, `vscode-anthropic`,
//!    `codex-cli`) plus an unknown name and an empty name.
//! 2. Run [`Harness::detect`] on that name (mirroring what
//!    `src/mcp.rs::serve_stdio` does on every `initialize` line — see
//!    `src/mcp.rs` lines 5052-5061).
//! 3. Call [`handle_capabilities_with_conn_v3`] with the detected
//!    harness — mirroring what the in-process MCP dispatcher does for
//!    the `memory_capabilities` tool.
//! 4. Assert the round-trip carries:
//!    - the expected detected harness identity,
//!    - the canonical `to_describe_to_user` first sentence (see
//!      `docs/v0.7/canonical-phrasings.md`),
//!    - the expected `your_harness_supports_deferred_registration`
//!      value (or absence, for the empty-name fallback),
//!    - the expected `agent_permitted_families` set under a known
//!      allowlist.
//!
//! The intent is to catch the regression where harness detection
//! silently breaks (e.g. a typo in `Harness::detect`'s match arms, a
//! removed variant, or a bypass of the harness threading in the
//! capabilities builder). Pure test additions — no schema changes, no
//! tool count changes; the surface stays at the v0.7.0 tool count
//! (51 tools — pinned in `src/profile.rs::Profile::full`).

use ai_memory::config::{FeatureTier, McpConfig, TierConfig};
use ai_memory::harness::Harness;
use ai_memory::mcp::handle_capabilities_with_conn_v3;
use ai_memory::profile::Profile;
use serde_json::{Value, json};
use std::collections::HashMap;

mod common;
use common::fresh_conn;

// ---------------------------------------------------------------------------
// Helpers — mirror the patterns in tests/capabilities_v3.rs so the two
// suites stay legible side by side.
// ---------------------------------------------------------------------------

/// Default tier for these tests. Matches `tests/capabilities_v3.rs`.
fn semantic_tier() -> TierConfig {
    FeatureTier::Semantic.config()
}

/// Build a minimal `[mcp.allowlist]` table for the `agent_permitted_families`
/// assertions. Mirrors `allowlist()` in `tests/capabilities_v3.rs`.
fn allowlist(rows: &[(&str, &[&str])]) -> McpConfig {
    let mut map = HashMap::new();
    for (agent, fams) in rows {
        map.insert(
            (*agent).to_string(),
            fams.iter().map(|s| (*s).to_string()).collect(),
        );
    }
    McpConfig {
        profile: None,
        allowlist: Some(map),
    }
}

/// Synthesize the `initialize` request body the MCP client would send
/// at handshake time and return the resolved [`Harness`] the substrate
/// would detect from it.
///
/// This mirrors the path in `src/mcp.rs::serve_stdio` (lines 5052-5061):
/// pull `clientInfo.name` out of `initialize.params` and feed it to
/// [`Harness::detect`]. A missing or empty name maps to `Generic("")`,
/// matching the unit-tested behavior in `src/harness.rs`.
fn detect_from_initialize(client_name: &str) -> Harness {
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": "0.0.0-test" }
        }
    });
    let name = init["params"]["clientInfo"]["name"]
        .as_str()
        .unwrap_or_default();
    Harness::detect(name)
}

/// Drive a `memory_capabilities` round-trip end-to-end:
/// detect the harness from the initialize, then build the v3
/// capabilities response with the detected harness threaded in.
///
/// Returns `(detected harness, v3 capabilities JSON)`.
fn round_trip(
    client_name: &str,
    profile: &Profile,
    mcp_config: Option<&McpConfig>,
    agent_id: Option<&str>,
) -> (Harness, Value) {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let harness = detect_from_initialize(client_name);
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        profile,
        mcp_config,
        agent_id,
        Some(&harness),
    )
    .expect("v3 capabilities serialize");
    (harness, val)
}

/// Canonical opening of `to_describe_to_user` under the `core` profile.
/// Pinned in `docs/v0.7/canonical-phrasings.md` and asserted in
/// `tests/capabilities_v3.rs::cap_v3_describe_core_profile_is_plain_english_with_loaded_names`.
/// We re-pin it here so a regression in the harness path that
/// accidentally swapped the profile or stripped the field would
/// surface as a D4 failure too (defense in depth).
const CORE_DESCRIBE_OPENING: &str = "I can directly use 7 memory tools right now (";

// ---------------------------------------------------------------------------
// D4 — five harness identity tests + one negative.
//
// Each named-harness test asserts:
//   1. `Harness::detect(<clientInfo.name>)` returns the expected
//      variant (catches regressions in the match arms).
//   2. `to_describe_to_user` opens with the canonical first sentence
//      pinned in docs/v0.7/canonical-phrasings.md (catches regressions
//      where harness threading accidentally clobbered the field).
//   3. `your_harness_supports_deferred_registration` carries the right
//      value per the compatibility matrix in docs/v0.7
//      (`true` only for Claude Code today; everything else `false`).
//   4. `agent_permitted_families` carries the expected family set for
//      a known allowlist + agent_id (catches regressions where the
//      A4 path silently lost the field when a harness was in scope).
// ---------------------------------------------------------------------------

/// D4 case 1 — `claude-code` is the canonical Claude Code harness name
/// and is the **only** harness today that supports deferred-tool
/// registration. The capabilities-v3 response must surface
/// `your_harness_supports_deferred_registration: true` for it.
#[test]
fn d4_claude_code_initialize_round_trip() {
    let cfg = allowlist(&[("alice", &["core", "graph"]), ("*", &["core"])]);
    let (harness, val) = round_trip("claude-code", &Profile::core(), Some(&cfg), Some("alice"));

    assert_eq!(
        harness,
        Harness::ClaudeCode,
        "claude-code clientInfo.name must detect as Harness::ClaudeCode"
    );

    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("to_describe_to_user must be present under v3");
    assert!(
        describe.starts_with(CORE_DESCRIBE_OPENING),
        "core profile describe must open canonically; got: {describe}"
    );

    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(Value::as_bool),
        Some(true),
        "Claude Code → field must be present and true; got: {val}"
    );

    let permitted = val["agent_permitted_families"]
        .as_array()
        .expect("agent_permitted_families must be present when allowlist enabled + agent_id given");
    let names: Vec<&str> = permitted.iter().filter_map(Value::as_str).collect();
    assert_eq!(
        names,
        vec!["core", "graph"],
        "alice → core + graph per the test allowlist; got: {names:?}"
    );
}

/// D4 case 2 — `cursor` is an eager-load MCP harness. Detection must
/// resolve to `Harness::Cursor` and the deferred-registration field
/// must be `false` (per the compatibility matrix in docs/v0.7).
#[test]
fn d4_cursor_initialize_round_trip() {
    let cfg = allowlist(&[("bob", &["core"]), ("*", &["core"])]);
    let (harness, val) = round_trip("cursor", &Profile::core(), Some(&cfg), Some("bob"));

    assert_eq!(harness, Harness::Cursor);

    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");
    assert!(
        describe.starts_with(CORE_DESCRIBE_OPENING),
        "got: {describe}"
    );

    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(Value::as_bool),
        Some(false),
        "Cursor is eager-load → field must be present and false; got: {val}"
    );

    let permitted = val["agent_permitted_families"]
        .as_array()
        .expect("agent_permitted_families present");
    let names: Vec<&str> = permitted.iter().filter_map(Value::as_str).collect();
    assert_eq!(names, vec!["core"], "bob → core only; got: {names:?}");
}

/// D4 case 3 — `vscode-anthropic` is **not** a recognised harness in
/// the current `Harness::detect` match arms (the recognised VS Code
/// harness is Cline, matched on `cline`). Per the
/// "unknown harness defaults conservative" contract in
/// `src/harness.rs`, this name must round-trip into
/// `Harness::Generic("vscode-anthropic")` and the
/// `your_harness_supports_deferred_registration` field must be `false`
/// (conservative default — the LLM should not promise mid-session tool
/// surfacing on an unknown harness). This test pins that contract so a
/// future PR that adds `vscode-anthropic` to the detection table will
/// have to update this expectation deliberately rather than silently.
#[test]
fn d4_vscode_anthropic_initialize_round_trip() {
    let cfg = allowlist(&[("eve", &["core"]), ("*", &["core"])]);
    let (harness, val) = round_trip(
        "vscode-anthropic",
        &Profile::core(),
        Some(&cfg),
        Some("eve"),
    );

    match &harness {
        Harness::Generic(s) => assert_eq!(
            s, "vscode-anthropic",
            "Generic must preserve the original clientInfo.name verbatim"
        ),
        other => panic!("expected Harness::Generic(\"vscode-anthropic\"); got {other:?}"),
    }

    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");
    assert!(
        describe.starts_with(CORE_DESCRIBE_OPENING),
        "got: {describe}"
    );

    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(Value::as_bool),
        Some(false),
        "Generic harness → field must be present and false (conservative); got: {val}"
    );

    let permitted = val["agent_permitted_families"]
        .as_array()
        .expect("agent_permitted_families present");
    let names: Vec<&str> = permitted.iter().filter_map(Value::as_str).collect();
    assert_eq!(names, vec!["core"], "eve → wildcard core; got: {names:?}");
}

/// D4 case 4 — `codex-cli` matches Codex via the substring rule in
/// `Harness::detect`. Codex is eager-load, so the deferred field must
/// be `false`.
#[test]
fn d4_codex_cli_initialize_round_trip() {
    let cfg = allowlist(&[
        ("alice", &["core", "graph"]),
        ("bob", &["core"]),
        ("*", &["core"]),
    ]);
    let (harness, val) = round_trip("codex-cli", &Profile::core(), Some(&cfg), Some("bob"));

    assert_eq!(
        harness,
        Harness::Codex,
        "codex-cli must detect as Harness::Codex via substring match"
    );

    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");
    assert!(
        describe.starts_with(CORE_DESCRIBE_OPENING),
        "got: {describe}"
    );

    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(Value::as_bool),
        Some(false),
        "Codex → field must be present and false; got: {val}"
    );

    let permitted = val["agent_permitted_families"]
        .as_array()
        .expect("agent_permitted_families present");
    let names: Vec<&str> = permitted.iter().filter_map(Value::as_str).collect();
    assert_eq!(
        names,
        vec!["core"],
        "bob → explicit row wins over wildcard; got: {names:?}"
    );
}

/// D4 case 5 — an unknown name that does not collide with any
/// substring in `Harness::detect` rounds-trips into Generic preserving
/// the original string. The deferred field stays `false`.
#[test]
fn d4_unknown_harness_initialize_round_trip() {
    let cfg = allowlist(&[("alice", &["core", "graph"]), ("*", &["core"])]);
    let raw = "MyCustomMcpClient/0.1";
    let (harness, val) = round_trip(raw, &Profile::core(), Some(&cfg), Some("alice"));

    match &harness {
        Harness::Generic(s) => assert_eq!(
            s, raw,
            "Generic must preserve the original clientInfo.name verbatim"
        ),
        other => panic!("expected Harness::Generic({raw:?}); got {other:?}"),
    }

    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");
    assert!(
        describe.starts_with(CORE_DESCRIBE_OPENING),
        "got: {describe}"
    );

    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(Value::as_bool),
        Some(false),
        "unknown harness → field must be present and false (conservative default); got: {val}"
    );

    let permitted = val["agent_permitted_families"]
        .as_array()
        .expect("agent_permitted_families present");
    let names: Vec<&str> = permitted.iter().filter_map(Value::as_str).collect();
    assert_eq!(
        names,
        vec!["core", "graph"],
        "alice → core + graph per allowlist; got: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// D4 negative case — empty `clientInfo.name` falls back to the
// generic/unknown bucket.
//
// `serve_stdio` defensively skips the harness-detection branch when
// `clientInfo.name` is missing or empty (see `src/mcp.rs` line 5054:
// `&& !name.is_empty()`), leaving `detected_harness = None`. The unit
// test in `src/harness.rs::detect_empty_name_is_generic` confirms that
// even if the empty string IS fed to detect, it maps to
// `Generic("")` rather than panicking.
//
// This integration test pins both halves of the contract:
//   1. `Harness::detect("")` returns `Generic("")` (defensive).
//   2. When the substrate honours its "skip detection on empty" rule
//      and threads `None` into the v3 capabilities builder, the
//      `your_harness_supports_deferred_registration` field is
//      OMITTED from the wire (per B4's `skip_serializing_if`). Absence
//      carries meaning distinct from `false`: false means "we know the
//      harness can't", absent means "we don't know the harness".
// ---------------------------------------------------------------------------
#[test]
fn d4_empty_client_info_name_falls_back_to_generic_and_omits_field() {
    // (1) Detect-side defensive behavior — empty string maps to
    //     Generic(""), which `supports_deferred_registration` reports as
    //     false.
    assert_eq!(Harness::detect(""), Harness::Generic(String::new()));
    assert!(
        !Harness::detect("").supports_deferred_registration(),
        "Generic(\"\") must default to false (conservative)"
    );

    // (2) Substrate-side "skip detection on empty" — the production
    //     `serve_stdio` only sets `detected_harness` when
    //     `clientInfo.name` is non-empty, so the v3 builder is invoked
    //     with `harness = None` and the field is omitted from the wire.
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None, // empty clientInfo.name → no harness threaded in
    )
    .expect("v3 capabilities serialize");

    assert!(
        val.get("your_harness_supports_deferred_registration")
            .is_none(),
        "empty clientInfo.name → field must be absent on wire (skip_serializing_if); got: {val}"
    );

    // The describe/summary fields still serialize (they don't depend
    // on the harness), so the rest of the v3 contract remains intact
    // even on a malformed handshake.
    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");
    assert!(
        describe.starts_with(CORE_DESCRIBE_OPENING),
        "describe must still serialize even with no harness; got: {describe}"
    );
    assert_eq!(val["schema_version"], "3");
}
