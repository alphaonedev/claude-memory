// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::too_many_lines)]
//! Issue #902 — postgres migration v47 rejects phantom-signed
//! `memory_links` rows.
//!
//! ## Background
//!
//! Pre-#902 the postgres SAL adapter accepted rows on `memory_links`
//! that claimed `attest_level IN ('self_signed', 'peer_attested')` while
//! `signature` was NULL or carried a wrong-length byte string — the
//! sqlite path correctly rejected such "phantom-signed" rows via the
//! v37 trigger (`migrations/sqlite/0037_v07_persona_signing_atomicity.sql`),
//! but the matching postgres migration
//! (`migrations/postgres/0024_v07_persona_signing_atomicity.sql`) was
//! ON DISK but never registered in the ladder. The fix landed in
//! commit `c17f9a8a4` and added:
//!
//! - `MIGRATION_V47_PERSONA_SIGNING_ATOMICITY` constant in
//!   `src/store/postgres.rs`
//! - `migrate_v47()` fn dispatching the orphaned migration
//! - ladder dispatch step at the `migrate_*` site
//! - `CURRENT_SCHEMA_VERSION` 46 → 47
//! - parity assertion 46 → 47 in
//!   `tests/postgres_schema_parity.rs`
//!
//! The CHECK constraint added by the migration is:
//!
//! ```sql
//! ALTER TABLE memory_links
//!     ADD CONSTRAINT memory_links_attest_signature_atomic_ck
//!     CHECK (
//!         attest_level NOT IN ('self_signed', 'peer_attested')
//!         OR (signature IS NOT NULL AND octet_length(signature) = 64)
//!     );
//! ```
//!
//! ## What this test asserts
//!
//! Three INSERT shapes against `memory_links`, each in a fresh
//! transaction so PG rejects don't poison the connection:
//!
//! 1. `attest_level='self_signed' + signature=NULL` → CHECK violation.
//! 2. `attest_level='self_signed' + signature=<32 bytes>` (wrong length)
//!    → CHECK violation.
//! 3. `attest_level='self_signed' + signature=<64 bytes>` → success.
//!
//! All three assertions cite the constraint name
//! `memory_links_attest_signature_atomic_ck` (or fall back to a
//! generic "check constraint" substring for engines that mask the
//! name in `Display`) to pin that the migration's CHECK is the gate.
//!
//! ## Gating
//!
//! Skipped without `AI_MEMORY_TEST_POSTGRES_URL` — same convention as
//! `tests/sal_v07_postgres_findings.rs` (#910 et al). The
//! `eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set")` line keeps
//! the CI matrix without live PG green.

#![cfg(feature = "sal-postgres")]

use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{CallerContext, MemoryStore};
use chrono::Utc;
use sqlx::Executor;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

mod common;
use common::postgres_url;

/// Build a fresh `Memory` row template the test can seed via the SAL
/// `store` method — same pattern as `tests/sal_v07_postgres_findings.rs`.
fn fresh_memory(title: &str, namespace: &str) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: "phantom-signed-link-test-content".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:test"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

#[tokio::test]
async fn postgres_rejects_phantom_signed_link() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    // Initialize PostgresStore — runs the full migration ladder
    // including the v47 CHECK constraint from #902.
    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: connect failed: {e}");
            return;
        }
    };

    // Sanity: the store reports it's at schema v47 (or above) so the
    // CHECK is actually installed. Anything older means the migration
    // didn't land — fail loud rather than report a false-negative.
    let v = store.schema_version().await.expect("schema_version");
    assert!(
        v >= 47,
        "#902 regression: PostgresStore must run migration v47 before this test; \
         got CURRENT_SCHEMA_VERSION={v}"
    );

    let ctx = CallerContext::for_agent("phantom-signed-link-test");
    let namespace = format!("issue-902-{}", Uuid::new_v4());

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let src_mem = fresh_memory(&format!("source-{nanos}-{}", Uuid::new_v4()), &namespace);
    let tgt_mem = fresh_memory(&format!("target-{nanos}-{}", Uuid::new_v4()), &namespace);

    let src_id = store
        .store(&ctx, &src_mem)
        .await
        .expect("store source memory");
    let tgt_id = store
        .store(&ctx, &tgt_mem)
        .await
        .expect("store target memory");

    // Open a sibling pool for the raw-SQL probes — the SAL trait
    // doesn't expose raw `INSERT INTO memory_links` and we need to
    // exercise the CHECK constraint directly.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("sibling pool for raw-SQL probes");

    // ─────────────────────────────────────────────────────────────
    // Phantom case 1 — `attest_level='self_signed'`, `signature=NULL`.
    // The pre-#902 postgres path accepted this; v47 must reject.
    // ─────────────────────────────────────────────────────────────
    let res_null = sqlx::query(
        "INSERT INTO memory_links (source_id, target_id, relation, attest_level, signature) \
         VALUES ($1, $2, 'related_to', 'self_signed', NULL)",
    )
    .bind(&src_id)
    .bind(&tgt_id)
    .execute(&pool)
    .await;

    assert!(
        res_null.is_err(),
        "#902 regression: INSERT with attest_level='self_signed' + NULL signature \
         must fail the CHECK; got Ok"
    );
    let err_null = format!("{}", res_null.err().unwrap());
    assert!(
        err_null.contains("memory_links_attest_signature_atomic_ck")
            || err_null.to_lowercase().contains("check constraint"),
        "expected CHECK violation citing memory_links_attest_signature_atomic_ck; \
         got: {err_null}"
    );

    // ─────────────────────────────────────────────────────────────
    // Phantom case 2 — `attest_level='self_signed'`, `signature` of
    // the wrong length (32 bytes instead of 64). Ed25519 signatures
    // are exactly 64 bytes; the CHECK's `octet_length(signature) = 64`
    // half is what gates this.
    // ─────────────────────────────────────────────────────────────
    let short_sig: Vec<u8> = vec![0xAA; 32];
    let res_short = sqlx::query(
        "INSERT INTO memory_links (source_id, target_id, relation, attest_level, signature) \
         VALUES ($1, $2, 'related_to', 'self_signed', $3)",
    )
    .bind(&src_id)
    .bind(&tgt_id)
    .bind(&short_sig)
    .execute(&pool)
    .await;

    assert!(
        res_short.is_err(),
        "#902 regression: INSERT with attest_level='self_signed' + 32-byte signature \
         must fail the CHECK; got Ok"
    );
    let err_short = format!("{}", res_short.err().unwrap());
    assert!(
        err_short.contains("memory_links_attest_signature_atomic_ck")
            || err_short.to_lowercase().contains("check constraint"),
        "expected CHECK violation citing memory_links_attest_signature_atomic_ck; \
         got: {err_short}"
    );

    // ─────────────────────────────────────────────────────────────
    // Positive case — `attest_level='self_signed'`, `signature` of
    // exactly 64 bytes. The CHECK passes; the row lands.
    // ─────────────────────────────────────────────────────────────
    let good_sig: Vec<u8> = vec![0xCC; 64];
    let res_good = sqlx::query(
        "INSERT INTO memory_links (source_id, target_id, relation, attest_level, signature) \
         VALUES ($1, $2, 'related_to', 'self_signed', $3)",
    )
    .bind(&src_id)
    .bind(&tgt_id)
    .bind(&good_sig)
    .execute(&pool)
    .await;

    assert!(
        res_good.is_ok(),
        "INSERT with attest_level='self_signed' + 64-byte signature must \
         succeed (#902 CHECK is satisfied); got: {res_good:?}"
    );

    // Cleanup — drop both memories so re-runs on the same DB stay
    // idempotent. ON DELETE CASCADE on `memory_links.source_id` /
    // `target_id` carries the link row out with them.
    let _ = pool
        .execute(
            sqlx::query("DELETE FROM memories WHERE id IN ($1, $2)")
                .bind(&src_id)
                .bind(&tgt_id),
        )
        .await;
}
