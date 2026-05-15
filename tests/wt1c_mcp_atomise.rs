// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-C — `memory_atomise` MCP tool acceptance suite.
//!
//! Eight tests pinned to the WT-1-C brief, all driving the handler
//! directly (no live LLM, no spawned MCP daemon). A deterministic
//! `MockCurator` is threaded through every test so the suite is
//! hermetic and fast.
//!
//! Test inventory (per the WT-1-C brief):
//!
//! 1. `test_memory_atomise_tool_registered` — tool name surfaces in
//!    `tool_definitions_for_profile(full)` and is absent from `core`.
//! 2. `test_memory_atomise_invokes_atomiser` — mock returns success,
//!    handler emits the documented `{source_id, atom_ids, atom_count,
//!    archived_at}` shape.
//! 3. `test_memory_atomise_keyword_tier_locked` — tier-locked advisory
//!    envelope for the keyword tier (informational, NOT a JSON-RPC
//!    error).
//! 4. `test_memory_atomise_already_atomised_returns_informational` —
//!    200 OK with `already_atomised: true` on a second call.
//! 5. `test_memory_atomise_source_too_small_returns_informational` —
//!    200 OK with `source_too_small: true`.
//! 6. `test_memory_atomise_curator_failure_returns_error` —
//!    MCP `isError: true` envelope with `CURATOR_FAILED:` prefix.
//! 7. `test_memory_atomise_governance_refusal_includes_index` — the
//!    refused atom index appears in the error envelope's body.
//! 8. `test_memory_atomise_input_validation` — `max_atom_tokens=0`
//!    fails; `force_re_atomise="yes"` fails.

#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]

use std::sync::{Arc, Mutex, OnceLock};

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::FeatureTier;
use ai_memory::mcp::tools::AtomiseToolHandler;
use ai_memory::mcp::tools::handle_atomise;
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::profile::Profile;
use ai_memory::storage;
use ai_memory::storage::GovernanceRefusal;

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Mock curator — deterministic, programmable, no network.
// ---------------------------------------------------------------------------

struct MockCurator {
    responses: Mutex<Vec<Result<Vec<Atom>, CuratorError>>>,
}

impl MockCurator {
    fn new(responses: Vec<Result<Vec<Atom>, CuratorError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Curator for MockCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(CuratorError::MalformedResponse(
                "mock: queue exhausted".into(),
            ));
        }
        q.remove(0)
    }
}

fn atoms(texts: &[&str]) -> Vec<Atom> {
    texts
        .iter()
        .map(|s| Atom {
            text: (*s).to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared governance-hook scaffolding — mirrors `tests/atomisation/core.rs`
// so this suite and the WT-1-B suite can coexist under `cargo test`
// without fighting over the `GOVERNANCE_PRE_WRITE` OnceLock.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum HookMode {
    Allow,
    RefuseAtomAtIndex { idx: usize, reason: String },
}

fn hook_mode_slot() -> &'static Mutex<HookMode> {
    static SLOT: OnceLock<Mutex<HookMode>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HookMode::Allow))
}

fn atomiser_atom_counter() -> &'static std::sync::atomic::AtomicUsize {
    static C: OnceLock<std::sync::atomic::AtomicUsize> = OnceLock::new();
    C.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
}

fn test_serial() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn ensure_hook_installed() {
    let _ = storage::GOVERNANCE_PRE_WRITE.set(Box::new(|mem: &Memory| {
        let guard = hook_mode_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            HookMode::Allow => Ok(()),
            HookMode::RefuseAtomAtIndex { idx, reason } => {
                if mem.source == "atomiser" {
                    let current =
                        atomiser_atom_counter().fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if current == *idx {
                        return Err(reason.clone());
                    }
                }
                Ok(())
            }
        }
    }));
}

fn set_mode(mode: HookMode) {
    *hook_mode_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    atomiser_atom_counter().store(0, std::sync::atomic::Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fresh_db() -> (NamedTempFile, Connection) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let conn = storage::open(tmp.path()).expect("db::open");
    (tmp, conn)
}

/// Insert a long-bodied source so the atomiser never short-circuits
/// with `SourceTooSmall`. Mirrors the helper in `tests/atomisation/
/// core.rs` — paragraph repetition pushes the cl100k count well above
/// the default 200-token budget.
fn insert_long_source(conn: &Connection, ns: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let body = (0..30)
        .map(|i| {
            format!(
                "Paragraph {i}: the kubernetes rolling deploy strategy required canary \
                 instance health checks. The pod readiness probe must pass before \
                 traffic shifts. Failures roll back the deployment within 30 seconds. \
                 Operator dashboards track replica counts and error rates."
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: format!("wt1c-source-{}", uuid::Uuid::new_v4().simple()),
        content: body,
        tags: vec!["kubernetes".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "test-agent"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    storage::insert(conn, &mem).expect("seed long source")
}

fn insert_short_source(conn: &Connection, ns: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: format!("wt1c-short-{}", uuid::Uuid::new_v4().simple()),
        content: "Short body that fits inside one atom budget.".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "test-agent"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    storage::insert(conn, &mem).expect("seed short source")
}

fn build_handler(
    responses: Vec<Result<Vec<Atom>, CuratorError>>,
    tier: FeatureTier,
) -> AtomiseToolHandler {
    let curator: Box<dyn Curator> = Box::new(MockCurator::new(responses));
    let atomiser = Arc::new(Atomiser::new(
        curator,
        None,
        AtomiserConfig::default(),
        tier,
    ));
    AtomiseToolHandler::new(atomiser, tier)
}

// ---------------------------------------------------------------------------
// Test 1 — tool registered in the full profile, absent in core.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_tool_registered() {
    // Full profile must surface the tool name.
    let full = ai_memory::mcp::tool_definitions_for_profile(&Profile::full());
    let tools = full["tools"].as_array().expect("tools array");
    let full_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(
        full_names.contains(&"memory_atomise"),
        "memory_atomise must be registered under --profile full; got: {full_names:?}"
    );

    // Core profile must NOT surface it (curator-pass tier-gated).
    let core = ai_memory::mcp::tool_definitions_for_profile(&Profile::core());
    let core_names: Vec<&str> = core["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        !core_names.contains(&"memory_atomise"),
        "memory_atomise must NOT be registered under --profile core; got: {core_names:?}"
    );

    // Power profile (where memory_consolidate / memory_reflect live)
    // must also include it.
    let power = ai_memory::mcp::tool_definitions_for_profile(&Profile::power());
    let power_names: Vec<&str> = power["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        power_names.contains(&"memory_atomise"),
        "memory_atomise must be registered under --profile power; got: {power_names:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — mock atomiser returns success, handler emits documented shape.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_invokes_atomiser() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();
    set_mode(HookMode::Allow);

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1c/invokes");

    let curator_atoms = atoms(&[
        "Atom A: canary instance health check must pass.",
        "Atom B: readiness probe gates traffic shift.",
        "Atom C: rollback fires within 30 seconds on failure.",
        "Atom D: operator dashboards track replica counts.",
    ]);
    let handler = build_handler(vec![Ok(curator_atoms)], FeatureTier::Smart);

    let resp = handle_atomise(
        &conn,
        &json!({"memory_id": source_id, "max_atom_tokens": 200}),
        Some(&handler),
        FeatureTier::Smart,
        Some("ai:wt1c-test"),
    )
    .expect("happy-path atomise must succeed");

    // Documented shape: {source_id, atom_ids, atom_count, archived_at}.
    assert_eq!(resp["source_id"].as_str(), Some(source_id.as_str()));
    let atom_ids = resp["atom_ids"].as_array().expect("atom_ids array");
    assert!(
        !atom_ids.is_empty(),
        "atom_ids must be non-empty on the happy path"
    );
    let atom_count = resp["atom_count"].as_u64().expect("atom_count u64");
    assert_eq!(atom_count, atom_ids.len() as u64);
    assert!(
        resp["archived_at"].is_string(),
        "archived_at must be RFC3339 string"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — keyword tier returns tier-locked advisory envelope.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_keyword_tier_locked() {
    let (_tmp, conn) = fresh_db();
    // No handler at the keyword tier (LLM unavailable).
    let resp = handle_atomise(
        &conn,
        &json!({"memory_id": "11111111-2222-3333-4444-555555555555"}),
        None,
        FeatureTier::Keyword,
        None,
    )
    .expect("tier-locked is informational, NOT an MCP error");

    assert_eq!(
        resp["tier-locked"].as_str(),
        Some("memory_atomise requires smart tier or higher"),
        "advisory must carry the canonical tier-locked sentence"
    );
    assert_eq!(resp["current_tier"].as_str(), Some("keyword"));
    assert_eq!(resp["required_tier"].as_str(), Some("smart"));
}

// ---------------------------------------------------------------------------
// Test 4 — second call returns the existing atom_ids as informational
// {already_atomised: true, existing_atom_ids: [...]} envelope.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_already_atomised_returns_informational() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();
    set_mode(HookMode::Allow);

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1c/already");

    // First call commits a fresh set of atoms.
    let handler_a = build_handler(
        vec![Ok(atoms(&[
            "Atom 1 first run.",
            "Atom 2 first run.",
            "Atom 3 first run.",
        ]))],
        FeatureTier::Smart,
    );
    let first = handle_atomise(
        &conn,
        &json!({"memory_id": source_id}),
        Some(&handler_a),
        FeatureTier::Smart,
        Some("ai:wt1c-test"),
    )
    .expect("first atomise");
    let first_ids: Vec<String> = first["atom_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(!first_ids.is_empty(), "first call must mint atoms");

    // Second call — handler reports already_atomised: true with the
    // existing ids. 200 OK (no error).
    let handler_b = build_handler(vec![Ok(atoms(&["should not be used"]))], FeatureTier::Smart);
    let second = handle_atomise(
        &conn,
        &json!({"memory_id": source_id}),
        Some(&handler_b),
        FeatureTier::Smart,
        Some("ai:wt1c-test"),
    )
    .expect("second atomise must be 200 OK informational");

    assert_eq!(second["already_atomised"], Value::Bool(true));
    let existing_ids: Vec<String> = second["existing_atom_ids"]
        .as_array()
        .expect("existing_atom_ids array")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    // Same ids as the first commit (the engine returns them ordered
    // by created_at — set equality is the relevant invariant here).
    assert_eq!(
        existing_ids.len(),
        first_ids.len(),
        "second call should report the same number of existing atoms"
    );
    for id in &first_ids {
        assert!(
            existing_ids.contains(id),
            "existing_atom_ids must contain {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 5 — short source returns {source_too_small: true} informational.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_source_too_small_returns_informational() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();
    set_mode(HookMode::Allow);

    let (_tmp, conn) = fresh_db();
    let source_id = insert_short_source(&conn, "wt1c/small");

    let handler = build_handler(vec![Ok(atoms(&["unused"]))], FeatureTier::Smart);
    let resp = handle_atomise(
        &conn,
        &json!({"memory_id": source_id, "max_atom_tokens": 200}),
        Some(&handler),
        FeatureTier::Smart,
        Some("ai:wt1c-test"),
    )
    .expect("source_too_small is informational, NOT an MCP error");

    assert_eq!(resp["source_too_small"], Value::Bool(true));
    assert!(
        resp["message"].is_string(),
        "informational envelope must carry an operator-readable message"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — curator failure collapses to MCP isError envelope with
// the CURATOR_FAILED discriminator + the parser diagnostic.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_curator_failure_returns_error() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();
    set_mode(HookMode::Allow);

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1c/curator-err");

    // Mock returns a curator error verbatim.
    let handler = build_handler(
        vec![Err(CuratorError::MalformedResponse(
            "parse failed: expected `atoms` field".into(),
        ))],
        FeatureTier::Smart,
    );
    let err = handle_atomise(
        &conn,
        &json!({"memory_id": source_id}),
        Some(&handler),
        FeatureTier::Smart,
        Some("ai:wt1c-test"),
    )
    .expect_err("curator failure must surface as MCP-handler Err");

    assert!(
        err.starts_with("CURATOR_FAILED:"),
        "discriminator must be `CURATOR_FAILED:`; got: {err}"
    );
    assert!(
        err.contains("parse failed"),
        "operator-visible diagnostic must be preserved; got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Test 7 — governance refusal mid-batch surfaces the refused atom index.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_governance_refusal_includes_index() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();
    // Refuse the third atom (idx=2). The first two land successfully;
    // the third triggers a GovernanceRefusal.
    let refusal_reason = "test-policy-refuses-atom-2";
    set_mode(HookMode::RefuseAtomAtIndex {
        idx: 2,
        reason: refusal_reason.to_string(),
    });

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1c/governance");

    let handler = build_handler(
        vec![Ok(atoms(&[
            "Atom alpha — accepted.",
            "Atom beta — accepted.",
            "Atom gamma — refused by hook.",
            "Atom delta — never reached.",
        ]))],
        FeatureTier::Smart,
    );
    let err = handle_atomise(
        &conn,
        &json!({"memory_id": source_id}),
        Some(&handler),
        FeatureTier::Smart,
        Some("ai:wt1c-test"),
    )
    .expect_err("governance refusal must surface as MCP-handler Err");

    assert!(
        err.starts_with("GOVERNANCE_REFUSED:"),
        "discriminator must be `GOVERNANCE_REFUSED:`; got: {err}"
    );
    assert!(
        err.contains("atom[2]"),
        "refused atom index must appear in the error body; got: {err}"
    );
    assert!(
        err.contains(refusal_reason),
        "operator-authored refusal reason must be preserved; got: {err}"
    );

    // Restore the hook mode so any sibling test that runs after us
    // doesn't see a stale refusal counter.
    set_mode(HookMode::Allow);
    // Force a use of the GovernanceRefusal symbol so the compiler
    // doesn't drop the import (kept to document the substrate-level
    // refusal type the engine internally downcasts).
    let _ = std::mem::size_of::<GovernanceRefusal>();
}

// ---------------------------------------------------------------------------
// Test 8 — input validation. max_atom_tokens=0 fails;
// force_re_atomise="yes" string fails.
// ---------------------------------------------------------------------------

#[test]
fn test_memory_atomise_input_validation() {
    let (_tmp, conn) = fresh_db();
    let handler = build_handler(vec![], FeatureTier::Smart);

    // max_atom_tokens=0 → out of range.
    let err = handle_atomise(
        &conn,
        &json!({
            "memory_id": "11111111-2222-3333-4444-555555555555",
            "max_atom_tokens": 0
        }),
        Some(&handler),
        FeatureTier::Smart,
        None,
    )
    .expect_err("max_atom_tokens=0 must be rejected");
    assert!(
        err.contains("out of range"),
        "max_atom_tokens=0 must surface the range diagnostic; got: {err}"
    );

    // force_re_atomise="yes" → type error (string is not bool).
    let err = handle_atomise(
        &conn,
        &json!({
            "memory_id": "11111111-2222-3333-4444-555555555555",
            "force_re_atomise": "yes"
        }),
        Some(&handler),
        FeatureTier::Smart,
        None,
    )
    .expect_err("force_re_atomise=\"yes\" must be rejected");
    assert!(
        err.contains("boolean"),
        "force_re_atomise=\"yes\" must surface the type diagnostic; got: {err}"
    );

    // Sanity: max_atom_tokens above range also fails (defense-in-depth
    // pin for the symmetric branch).
    let err = handle_atomise(
        &conn,
        &json!({
            "memory_id": "11111111-2222-3333-4444-555555555555",
            "max_atom_tokens": 1_001
        }),
        Some(&handler),
        FeatureTier::Smart,
        None,
    )
    .expect_err("max_atom_tokens=1001 must be rejected");
    assert!(err.contains("out of range"));
}
