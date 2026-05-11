// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::cast_possible_wrap)]
//! v0.7.0 #628 I1 (review blocker H7) — zstd decompression-bomb defence.
//!
//! `transcripts::fetch` previously called `zstd::stream::read::Decoder`
//! into an unbounded `Vec<u8>`, so a hostile blob (e.g. 1 KB compressed
//! → multi-GB decompressed) could OOM the daemon. This test crafts a
//! blob whose decompressed length exceeds
//! [`ai_memory::transcripts::MAX_DECOMPRESSED_BYTES`] and asserts:
//!
//! 1. `fetch` returns an error rather than allocating gigabytes.
//! 2. The error message names the cap so an operator triaging the
//!    structured-log line knows immediately why the read failed.
//! 3. The daemon stays up — the surrounding process is not crashed by
//!    the rejection (asserted implicitly: the test process keeps
//!    running and a subsequent legitimate `fetch` still succeeds).

use std::io::Write;

use ai_memory::db;
use ai_memory::transcripts::{self, MAX_DECOMPRESSED_BYTES};
use rusqlite::params;

/// Produce a zstd-compressed blob whose decompressed length is
/// `target` bytes. Compresses a long run of one repeated byte so the
/// ratio is enormous — a few KB compressed pays for a multi-MB
/// decompressed payload.
fn make_bomb(target_decompressed: usize) -> Vec<u8> {
    // zstd-3 to match what `transcripts::store` writes on the happy
    // path. The decoder doesn't care about the level; the encoder
    // version is what produces the dictionary bytes the decoder
    // expects to consume.
    let raw = vec![0u8; target_decompressed];
    let mut out: Vec<u8> = Vec::with_capacity(8192);
    {
        let mut encoder = zstd::stream::write::Encoder::new(&mut out, 3).expect("encoder");
        encoder.write_all(&raw).expect("write_all");
        encoder.finish().expect("finish");
    }
    out
}

/// Insert a transcript row directly with a hand-crafted blob,
/// bypassing `transcripts::store` (which only ingests `&str`). This
/// is the only way to land a "bomb" row — the production write path
/// would never construct one.
fn insert_bomb(conn: &rusqlite::Connection, id: &str, blob: &[u8], original_size: i64) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_transcripts (
            id, namespace, created_at, expires_at,
            compressed_size, original_size, zstd_level, content_blob
         ) VALUES (?1, 'attack/lab', ?2, NULL, ?3, ?4, 3, ?5)",
        params![
            id,
            now,
            i64::try_from(blob.len()).unwrap(),
            original_size,
            blob,
        ],
    )
    .unwrap();
}

#[test]
fn fetch_rejects_blob_decompressing_past_cap() {
    // Aim for 4 MiB ABOVE the cap so we cleanly cross the threshold
    // without forcing the test machine to allocate the entire
    // post-cap payload to begin with. zstd-3 over an all-zero buffer
    // produces a tiny compressed blob (~kilobytes), so this is cheap
    // to build but unambiguously above the 16 MiB ceiling.
    let target = MAX_DECOMPRESSED_BYTES + 4 * 1024 * 1024;
    let bomb = make_bomb(target);
    assert!(
        bomb.len() < 256 * 1024,
        "test bomb should be small compressed (<256 KB) — got {} bytes",
        bomb.len()
    );

    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    insert_bomb(&conn, "bomb-1", &bomb, target as i64);

    let err = transcripts::fetch(&conn, "bomb-1").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("cap") || msg.contains("decompression"),
        "error must name the cap or decompression context, got: {msg}"
    );
}

#[test]
fn fetch_below_cap_still_round_trips_after_bomb_attempt() {
    // Compose two transcripts in one DB: a bomb (rejected) and a
    // legitimate small one. The legitimate read must succeed AFTER
    // the bomb attempt — a regression where the cap-check left the
    // decoder in a poisoned state would surface as the second fetch
    // also failing.
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();

    let bomb = make_bomb(MAX_DECOMPRESSED_BYTES + 1024 * 1024);
    insert_bomb(
        &conn,
        "bomb-2",
        &bomb,
        (MAX_DECOMPRESSED_BYTES + 1024 * 1024) as i64,
    );

    // Legitimate write through the public API.
    let legit_body = "hello legitimate transcript";
    let handle = transcripts::store(&conn, "team/eng", legit_body, None).unwrap();

    // Fire the bomb fetch first to exercise the rejection path.
    let _err = transcripts::fetch(&conn, "bomb-2").unwrap_err();

    // Then prove the legit fetch still works.
    let got = transcripts::fetch(&conn, &handle.id).unwrap();
    assert_eq!(got.as_deref(), Some(legit_body));
}

#[test]
fn fetch_respects_cap_constant_value() {
    // Pin the cap at exactly 16 MiB so a future change to the
    // constant trips the test AND the operator notices via the
    // commit diff. 16 MiB matches the v0.7.0 #628 H7 fix spec.
    assert_eq!(MAX_DECOMPRESSED_BYTES, 16 * 1024 * 1024);
}
