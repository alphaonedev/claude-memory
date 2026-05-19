// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]

//! v0.7.0 Provenance Gap 2 (issue #906) — end-to-end `source_uri`
//! update regression pin.
//!
//! `memory_update` (MCP) + `PUT /api/v1/memories/{id}` (HTTP) advertise
//! a `source_uri` patch field. Pre-#906 the storage layer dropped the
//! value silently — the field appeared on the schema, validated on the
//! way in, then never reached SQL. This test pins the four end-to-end
//! contracts:
//!
//! a) Rename: stored with `source_uri="doc:old.pdf"`, updated to
//!    `doc:new.pdf`, recall reports `doc:new.pdf`.
//! b) First-write: stored with `source_uri=None`, updated to
//!    `uri:https://example.com/x`, recall reports the URI.
//! c) No-op preservation: an update that does NOT touch source_uri
//!    leaves the stored value alone (COALESCE semantics).
//! d) Validation: an invalid source_uri (no recognised scheme prefix)
//!    fails before the storage layer is reached. The MCP surface and
//!    the HTTP `validate_update` both refuse such a patch.

use ai_memory::db;
use ai_memory::models::{Memory, Tier, UpdateMemory};
use ai_memory::validate;
use std::path::Path;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory(title: &str, source_uri: Option<&str>) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
        namespace: "source-uri-test".to_string(),
        tier: Tier::Mid,
        created_at: now.clone(),
        updated_at: now,
        source_uri: source_uri.map(str::to_string),
        ..Default::default()
    }
}

/// (a) Rename: an existing source_uri is rewritten through the storage
/// layer and the new value is observable on the next read. Mirrors the
/// "doc rename" use case the issue calls out.
#[test]
fn source_uri_rename_is_persisted_and_readable() {
    let conn = open_test_db();
    let mem = make_memory("rename-target", Some("doc:old.pdf"));
    let id = db::insert(&conn, &mem).expect("insert");

    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(
        stored.source_uri.as_deref(),
        Some("doc:old.pdf"),
        "seed must land with the original source_uri"
    );

    let (found, _) = db::update_with_expected_version(
        &conn,
        &id,
        None, // title
        None, // content
        None, // tier
        None, // namespace
        None, // tags
        None, // priority
        None, // confidence
        None, // expires_at
        None, // metadata
        Some("doc:new.pdf"),
        None, // expected_version
    )
    .expect("update with new source_uri");
    assert!(found, "update must locate the row");

    let after = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(
        after.source_uri.as_deref(),
        Some("doc:new.pdf"),
        "recall reports the rewritten source_uri"
    );
}

/// (b) First-write: a row that was stored without a source_uri can
/// have one added through an update. Covers the URI-scheme-migration
/// case where legacy rows carry `source_uri=NULL` and need to be
/// stamped after a fact-provenance backfill.
#[test]
fn source_uri_first_write_promotes_null_to_uri() {
    let conn = open_test_db();
    let mem = make_memory("legacy-row", None);
    let id = db::insert(&conn, &mem).expect("insert");

    let stored = db::get(&conn, &id).expect("get").expect("present");
    assert!(
        stored.source_uri.is_none(),
        "legacy row must land with source_uri=NULL"
    );

    let (found, _) = db::update_with_expected_version(
        &conn,
        &id,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("uri:https://example.com/article"),
        None,
    )
    .expect("update");
    assert!(found);

    let after = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(
        after.source_uri.as_deref(),
        Some("uri:https://example.com/article"),
        "first-write source_uri lands on the row"
    );
}

/// (c) No-op preservation: an update that does NOT touch source_uri
/// leaves the stored value alone. The COALESCE in the SQL UPDATE is
/// the load-bearing guard — a misimplementation that bound the param
/// directly (no COALESCE) would blank the column whenever the patch
/// didn't carry a URI.
#[test]
fn source_uri_unchanged_when_patch_omits_it() {
    let conn = open_test_db();
    let mem = make_memory("preserve", Some("uri:https://example.com/keep"));
    let id = db::insert(&conn, &mem).expect("insert");

    // Update only the title; source_uri patch is None.
    let (found, _) = db::update_with_expected_version(
        &conn,
        &id,
        Some("preserve-renamed"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None, // source_uri = None — must NOT blank the stored column
        None,
    )
    .expect("update");
    assert!(found);

    let after = db::get(&conn, &id).expect("get").expect("present");
    assert_eq!(after.title, "preserve-renamed");
    assert_eq!(
        after.source_uri.as_deref(),
        Some("uri:https://example.com/keep"),
        "update with source_uri=None must preserve the existing value (COALESCE)"
    );
}

/// (d) Validation: an invalid source_uri (no recognised scheme prefix
/// — `validate::VALID_SOURCE_URI_SCHEMES` lists the accepted prefixes)
/// is rejected before the storage layer is reached. Pinned at two
/// surfaces:
///
///   * `validate::validate_source_uri` directly — the lowest layer.
///   * `validate::validate_update` via `UpdateMemory.source_uri` —
///     the surface the HTTP `PUT /api/v1/memories/{id}` handler runs
///     before calling `db::update_with_expected_version`. A 400
///     BAD_REQUEST envelope is produced from this Err in
///     `src/handlers/memories.rs::update_memory` at the
///     `validate::validate_update(&body)` call.
#[test]
fn invalid_source_uri_rejected_by_validator() {
    // Direct surface — the MCP handler runs this verbatim.
    let direct = validate::validate_source_uri("no-scheme-here");
    assert!(
        direct.is_err(),
        "an opaque string with no scheme prefix must fail validation"
    );
    let msg = format!("{}", direct.unwrap_err());
    assert!(
        msg.contains("scheme") || msg.contains("must start with"),
        "diagnostic must name the missing scheme: got '{msg}'"
    );

    // HTTP-handler surface — `validate_update` is the gate that runs
    // before `db::update_with_expected_version` in the handler.
    let bad_update = UpdateMemory {
        title: None,
        content: None,
        tier: None,
        namespace: None,
        tags: None,
        priority: None,
        confidence: None,
        expires_at: None,
        metadata: None,
        source_uri: Some("no-scheme-here".to_string()),
    };
    let via_update = validate::validate_update(&bad_update);
    assert!(
        via_update.is_err(),
        "validate_update must reject an invalid source_uri before any storage write"
    );
}

/// Supersede path also threads source_uri — `update_with_archive_on_supersede`
/// is the LLM/Hook write surface that produces a fresh row with the
/// caller-supplied source_uri (or inherits when omitted).
#[test]
fn source_uri_threads_through_supersede_path() {
    let conn = open_test_db();
    let mem = make_memory("supersede-src", Some("doc:v1.pdf"));
    let id = db::insert(&conn, &mem).expect("insert");

    let result = db::update_with_archive_on_supersede(
        &conn,
        &id,
        None,
        Some("new body"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("doc:v2.pdf"),
        None,
        ai_memory::models::EditSource::Llm,
    )
    .expect("supersede");

    let new_row = db::get(&conn, &result.new_id)
        .expect("get")
        .expect("present");
    assert_eq!(
        new_row.source_uri.as_deref(),
        Some("doc:v2.pdf"),
        "supersede must carry the caller-supplied source_uri onto the new row"
    );

    // Inheritance: when source_uri is omitted on supersede, the new row
    // inherits the OLD row's value.
    let mem2 = make_memory("supersede-inherit", Some("doc:keep.pdf"));
    let id2 = db::insert(&conn, &mem2).expect("insert");
    let result2 = db::update_with_archive_on_supersede(
        &conn,
        &id2,
        None,
        Some("rewritten"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None, // source_uri omitted — must inherit from OLD row
        None,
        ai_memory::models::EditSource::Llm,
    )
    .expect("supersede inherit");
    let new_row2 = db::get(&conn, &result2.new_id)
        .expect("get")
        .expect("present");
    assert_eq!(
        new_row2.source_uri.as_deref(),
        Some("doc:keep.pdf"),
        "supersede with source_uri=None must inherit from the OLD row"
    );
}
