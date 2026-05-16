// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7-polish #783 — COV-17: skills federation round-trip.
//!
//! End-to-end: register a skill (with resources) on a local node →
//! export it to a folder (the skill wire format on disk) → import the
//! folder into a fresh "peer" database → assert that the peer can
//! resolve the skill's body and every attached resource with the
//! digest verified.
//!
//! ## Why this isn't run against the federation/* substrate
//!
//! As of `polish/v0.7-783` HEAD `d856df2`, the federation receive
//! pipeline (`src/federation/receive.rs`) ferries `memories` and
//! `memory_links` between peers — it does NOT yet carry the `skills`
//! / `skill_resources` tables. There is therefore no wire-level
//! "import-into-peer" handler to test directly.
//!
//! The substrate-side primitive that DOES exist is the export folder
//! shape: every skill round-trips through `target_folder/SKILL.md +
//! target_folder/resources/...` with the digest preserved (see
//! `tests/skill_test.rs::export_roundtrip_identical_digest`). The
//! folder is the on-the-wire shape an operator (or a future
//! federation skill-fanout) ships between peers — typically via
//! `tar | curl | tar -x` or a future `memory_skill_publish` tool.
//!
//! This test exercises that folder as the wire format: writer-side
//! `handle_skill_export` produces it from node A's DB; reader-side
//! `handle_skill_register` consumes it into node B's DB; resource
//! resolution on node B succeeds with the digest matching.
//!
//! If a future PR widens federation/* to fan skills out across peers,
//! this test stays valid: it pins the substrate guarantee (folder is
//! the canonical interchange shape) that the federation surface will
//! ride on top of.

#![allow(
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::map_unwrap_or
)]

use std::path::PathBuf;

use ai_memory::db;
use ai_memory::mcp::{handle_skill_export, handle_skill_register, handle_skill_resource};
use serde_json::{Value, json};

fn local_runs_root() -> PathBuf {
    std::env::var("TMPDIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local-runs")
                .join("tmp")
        })
}

/// Fresh DB at a unique path under TMPDIR. We use real files (not
/// `:memory:`) so two distinct peers don't accidentally share an
/// in-memory connection cache.
fn fresh_db_at(label: &str) -> (rusqlite::Connection, PathBuf) {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    let path = root.join(format!("cov17-{}-{}.db", label, uuid::Uuid::new_v4()));
    let conn = db::open(&path).expect("db::open");
    (conn, path)
}

const SKILL_BODY: &str = "# Round-trip Skill\n\n\
This is the body of the skill that the peer must end up with byte-\
identical after the export → import round-trip.\n\n\
## How it's used\n\n\
The peer reads this body via `memory_skill_get` after import.\n";

const RESOURCE_SCRIPT: &str = "#!/bin/bash\nset -euo pipefail\necho 'hello from peer'\n";
const RESOURCE_REFERENCE: &str = "# Reference notes\n\nDetails the peer needs.\n";

/// Build a SKILL.md folder layout under `root` with one script + one
/// reference resource. This is the input shape `handle_skill_register`
/// consumes (`folder_path`).
fn write_authored_skill_folder(root: &std::path::Path) {
    std::fs::create_dir_all(root).expect("mkdir root");
    std::fs::write(
        root.join("SKILL.md"),
        format!(
            "---\nnamespace: cov17\nname: round-trip-skill\n\
             description: A skill exercised through the export-import wire format.\n\
             ---\n\n{SKILL_BODY}"
        )
        .as_bytes(),
    )
    .expect("write SKILL.md");
    let scripts = root.join("resources").join("scripts");
    std::fs::create_dir_all(&scripts).expect("mkdir scripts");
    std::fs::write(scripts.join("run.sh"), RESOURCE_SCRIPT.as_bytes()).expect("write run.sh");
    let refer = root.join("resources").join("reference");
    std::fs::create_dir_all(&refer).expect("mkdir reference");
    std::fs::write(refer.join("notes.md"), RESOURCE_REFERENCE.as_bytes()).expect("write notes.md");
}

/// Full round-trip test: register a skill with resources on node A,
/// export to a folder, import on node B, assert resources resolve.
#[test]
fn skill_export_import_roundtrip_resolves_resources_on_peer() {
    // -----------------------------------------------------------------
    // 1. Build authored skill folder (the operator's source artifact).
    // -----------------------------------------------------------------
    let workspace = local_runs_root().join(format!("cov17-ws-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    let authored = workspace.join("authored-skill");
    write_authored_skill_folder(&authored);

    // -----------------------------------------------------------------
    // 2. Node A — local peer registers the skill from the folder.
    // -----------------------------------------------------------------
    let (conn_a, _path_a) = fresh_db_at("node-a");
    let registered_a: Value = handle_skill_register(
        &conn_a,
        &json!({"folder_path": authored.to_str().unwrap()}),
        None,
    )
    .expect("node A: register");
    assert_eq!(registered_a["registered"], json!(true));
    assert_eq!(registered_a["namespace"], json!("cov17"));
    assert_eq!(registered_a["name"], json!("round-trip-skill"));
    let skill_id_a = registered_a["id"]
        .as_str()
        .expect("skill id on node A")
        .to_string();
    let digest_a = registered_a["digest"]
        .as_str()
        .expect("digest on node A")
        .to_string();
    assert_eq!(digest_a.len(), 64, "digest must be 64-char hex");

    // -----------------------------------------------------------------
    // 3. Node A — export to a peer-transport folder (the wire format).
    // -----------------------------------------------------------------
    let exported = workspace.join("exported");
    let exported_resp = handle_skill_export(
        &conn_a,
        &json!({
            "skill_id": skill_id_a,
            "target_folder": exported.to_str().unwrap(),
        }),
        None,
    )
    .expect("node A: export");
    assert_eq!(exported_resp["exported"], json!(true));
    assert_eq!(
        exported_resp["digest"].as_str().unwrap_or(""),
        digest_a,
        "exported digest must match the registered digest",
    );
    // Exported folder must contain SKILL.md and the two resource files.
    assert!(
        exported.join("SKILL.md").is_file(),
        "wire format must include SKILL.md",
    );
    assert!(
        exported
            .join("resources")
            .join("scripts")
            .join("run.sh")
            .is_file(),
        "wire format must include scripts/run.sh",
    );
    assert!(
        exported
            .join("resources")
            .join("reference")
            .join("notes.md")
            .is_file(),
        "wire format must include reference/notes.md",
    );

    // -----------------------------------------------------------------
    // 4. Node B (the "peer") — fresh DB; no prior knowledge of node A.
    //    Import the exported folder. This is the substrate-side stand-
    //    in for the future federation receive handler.
    // -----------------------------------------------------------------
    let (conn_b, _path_b) = fresh_db_at("node-b");
    // Peer starts empty.
    let pre_count: i64 = conn_b
        .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pre_count, 0, "peer DB starts with zero skills");

    let registered_b: Value = handle_skill_register(
        &conn_b,
        &json!({"folder_path": exported.to_str().unwrap()}),
        None,
    )
    .expect("node B: register from peer transport");
    assert_eq!(registered_b["registered"], json!(true));
    let skill_id_b = registered_b["id"]
        .as_str()
        .expect("skill id on node B")
        .to_string();
    let digest_b = registered_b["digest"]
        .as_str()
        .expect("digest on node B")
        .to_string();

    // -----------------------------------------------------------------
    // 5. Wire-format invariant: digest is identical across peers. This
    //    is the load-bearing guarantee — the same skill on two peers
    //    is identifiable by the same content-address.
    // -----------------------------------------------------------------
    assert_eq!(
        digest_a, digest_b,
        "round-trip across peer transport must preserve digest exactly",
    );

    // -----------------------------------------------------------------
    // 6. Resource resolution on the peer — both attached resources
    //    must decompress + digest-verify successfully on node B,
    //    proving the substrate's resource interchange shape works
    //    when the importer was not the original author.
    // -----------------------------------------------------------------
    let script_resp = handle_skill_resource(
        &conn_b,
        &json!({
            "skill_id": skill_id_b,
            "resource_path": "scripts/run.sh",
        }),
    )
    .expect("peer: resolve script resource");
    assert_eq!(script_resp["digest_verified"], json!(true));
    assert_eq!(script_resp["encoding"], json!("utf-8"));
    assert_eq!(
        script_resp["content"].as_str().unwrap_or(""),
        RESOURCE_SCRIPT,
        "script content must be byte-identical on the peer",
    );
    assert_eq!(script_resp["resource_kind"], json!("script"));

    let reference_resp = handle_skill_resource(
        &conn_b,
        &json!({
            "skill_id": skill_id_b,
            "resource_path": "reference/notes.md",
        }),
    )
    .expect("peer: resolve reference resource");
    assert_eq!(reference_resp["digest_verified"], json!(true));
    assert_eq!(
        reference_resp["content"].as_str().unwrap_or(""),
        RESOURCE_REFERENCE,
        "reference content must be byte-identical on the peer",
    );
    assert_eq!(reference_resp["resource_kind"], json!("reference"));

    // -----------------------------------------------------------------
    // 7. Skill body round-trips. The peer's decompressed body must
    //    match the originally-authored body byte-for-byte.
    // -----------------------------------------------------------------
    let body_blob_b: Vec<u8> = conn_b
        .query_row(
            "SELECT body_blob FROM skills WHERE id = ?1",
            [&skill_id_b],
            |r| r.get(0),
        )
        .expect("read body blob on peer");
    let body_b = zstd::decode_all(body_blob_b.as_slice()).expect("decompress body on peer");
    assert_eq!(
        String::from_utf8_lossy(&body_b).into_owned(),
        SKILL_BODY,
        "peer-side body must be byte-identical to author-side body",
    );

    // Cleanup test scratch.
    let _ = std::fs::remove_dir_all(&workspace);
}

/// Sanity guard — the exported folder must register on the peer even
/// when the peer's DB was opened independently (no shared connection,
/// no shared in-memory state). Mirrors the network-transit shape: the
/// only thing carried between peers is the folder bytes.
#[test]
fn skill_export_folder_is_self_contained_for_peer_import() {
    let workspace = local_runs_root().join(format!("cov17-sc-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workspace).expect("mkdir workspace");
    let authored = workspace.join("authored");
    write_authored_skill_folder(&authored);

    let (conn_a, _) = fresh_db_at("self-contained-a");
    let resp_a = handle_skill_register(
        &conn_a,
        &json!({"folder_path": authored.to_str().unwrap()}),
        None,
    )
    .expect("register on a");
    let id_a = resp_a["id"].as_str().unwrap().to_string();

    let exported = workspace.join("exported");
    handle_skill_export(
        &conn_a,
        &json!({
            "skill_id": id_a,
            "target_folder": exported.to_str().unwrap(),
        }),
        None,
    )
    .expect("export from a");

    // Drop node A entirely before opening node B — proves the folder
    // alone carries the wire-format payload.
    drop(conn_a);

    let (conn_b, _) = fresh_db_at("self-contained-b");
    let resp_b = handle_skill_register(
        &conn_b,
        &json!({"folder_path": exported.to_str().unwrap()}),
        None,
    )
    .expect("register on b from exported folder alone");
    assert_eq!(resp_b["registered"], json!(true));
    assert_eq!(
        resp_b["digest"].as_str().unwrap_or(""),
        resp_a["digest"].as_str().unwrap_or("ZZZ"),
        "peer-import digest must match author-side digest",
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
