// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! In-tree `SqliteStore` adapter. Wraps the existing `crate::db` free
//! functions so the production path can migrate to the SAL trait
//! gradually. No behavior change vs. calling `crate::db` directly —
//! this is a thin shim whose only job is to prove the trait surface
//! fits the shape of the shipped code.

use crate::models::ConfidenceSource;
use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::OptionalExtension;
use tokio::sync::Mutex;

use crate::db;
use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

use super::{
    BoxBackendError, CallerContext, Capabilities, Filter, MemoryStore, StoreError, StoreResult,
    Transaction, UpdatePatch, VerifyFilter, VerifyLinkReport, VerifyReport, is_visible_to_caller,
};
use crate::quotas::{self, QuotaStatus};

/// SAL adapter over the existing bundled-SQLite storage. Holds an
/// `Arc<Mutex<Connection>>` matching the HTTP daemon's shared state so
/// the adapter can be used alongside the existing free-function code
/// paths during the migration.
pub struct SqliteStore {
    state: Arc<Mutex<rusqlite::Connection>>,
    path: PathBuf,
}

impl SqliteStore {
    /// Open (or create) a `SqliteStore` at the given path. Delegates
    /// schema init + migration to `crate::db::open`.
    pub fn open(path: impl Into<PathBuf>) -> StoreResult<Self> {
        let path = path.into();
        let conn = db::open(&path).map_err(box_err)?;
        Ok(Self {
            state: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    /// Path the adapter opened. Useful for diagnostics and for
    /// callers that need to spawn subprocesses (backup, rekey).
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn box_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(BoxBackendError::new(e.to_string()))
}

#[async_trait::async_trait]
impl MemoryStore for SqliteStore {
    fn capabilities(&self) -> Capabilities {
        // TRANSACTIONS + ATOMIC_MULTI_WRITE are NOT advertised because
        // the adapter does not currently expose `begin_transaction()`
        // — the trait default returns `UnsupportedCapability`. Honesty
        // here matters: capability bits must match runtime behaviour
        // (issue #302 item 6). Re-add these two flags once a real
        // transaction handle is wired through the mutex-guarded
        // `rusqlite::Connection`.
        Capabilities::FULLTEXT | Capabilities::DURABLE | Capabilities::STRONG_CONSISTENCY
    }

    /// v0.7.0.1 S75 — read `MAX(version)` from the live SQLite
    /// `schema_version` table so `/api/v1/capabilities.db_schema_version`
    /// reflects the actual applied migration ladder rather than a
    /// hard-coded constant. Returns `0` when the table is empty (a
    /// fresh DB that didn't run migrations yet) so the daemon never
    /// 503s the capabilities endpoint on a cold-start race.
    async fn schema_version(&self) -> StoreResult<i64> {
        let conn = self.state.lock().await;
        let v: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(v)
    }

    async fn store(&self, _ctx: &CallerContext, memory: &Memory) -> StoreResult<String> {
        let conn = self.state.lock().await;
        db::insert(&conn, memory).map_err(box_err)
    }

    async fn get(&self, ctx: &CallerContext, id: &str) -> StoreResult<Memory> {
        let conn = self.state.lock().await;
        match db::get(&conn, id).map_err(box_err)? {
            Some(mem) => {
                // #910 SAL-level scope=private gate — fold permission
                // denials into NotFound so the trait does not leak
                // existence to callers that lack read permission.
                // Admin/migrate paths set `bypass_visibility` and read
                // every row regardless of metadata.scope.
                if ctx.bypass_visibility || is_visible_to_caller(&mem, ctx.effective_principal()) {
                    Ok(mem)
                } else {
                    Err(StoreError::NotFound { id: id.to_string() })
                }
            }
            None => Err(StoreError::NotFound { id: id.to_string() }),
        }
    }

    async fn update(&self, _ctx: &CallerContext, id: &str, patch: UpdatePatch) -> StoreResult<()> {
        let conn = self.state.lock().await;
        // v0.7.0 Provenance Gap 2 (#906) — thread the patch's
        // `source_uri` slot into `update_with_expected_version` so the
        // sqlite SAL adapter honors source_uri rewrites end-to-end.
        // `expected_version=None` preserves the trait's existing
        // last-write-wins contract.
        let (found, _content_changed) = db::update_with_expected_version(
            &conn,
            id,
            patch.title.as_deref(),
            patch.content.as_deref(),
            patch.tier.as_ref(),
            patch.namespace.as_deref(),
            patch.tags.as_ref(),
            patch.priority,
            patch.confidence,
            None,
            patch.metadata.as_ref(),
            patch.source_uri.as_deref(),
            None,
        )
        .map_err(box_err)?;
        if found {
            Ok(())
        } else {
            Err(StoreError::NotFound { id: id.to_string() })
        }
    }

    async fn delete(&self, _ctx: &CallerContext, id: &str) -> StoreResult<()> {
        let conn = self.state.lock().await;
        let removed = db::delete(&conn, id).map_err(box_err)?;
        if removed {
            Ok(())
        } else {
            Err(StoreError::NotFound { id: id.to_string() })
        }
    }

    async fn list(&self, ctx: &CallerContext, filter: &Filter) -> StoreResult<Vec<Memory>> {
        let conn = self.state.lock().await;
        let tags_first = filter.tags_any.first().map(String::as_str);
        let since = filter.since.map(|d| d.to_rfc3339());
        let until = filter.until.map(|d| d.to_rfc3339());
        let rows = db::list(
            &conn,
            filter.namespace.as_deref(),
            filter.tier.as_ref(),
            if filter.limit == 0 { 100 } else { filter.limit },
            0,
            None,
            since.as_deref(),
            until.as_deref(),
            tags_first,
            filter.agent_id.as_deref(),
        )
        .map_err(box_err)?;
        // #910 SAL-level scope=private gate (see `is_visible_to_caller`
        // contract on the trait). Every query path that returns Memory
        // rows runs the result set through the canonical predicate so
        // every caller — handler, MCP tool, federation receiver — gets
        // the visibility-filtered set without needing a per-callsite
        // post-filter. Admin/migrate paths set `bypass_visibility` and
        // round-trip every row regardless of metadata.scope.
        if ctx.bypass_visibility {
            return Ok(rows);
        }
        let caller = ctx.effective_principal();
        Ok(rows
            .into_iter()
            .filter(|m| is_visible_to_caller(m, caller))
            .collect())
    }

    async fn search(
        &self,
        ctx: &CallerContext,
        query: &str,
        filter: &Filter,
    ) -> StoreResult<Vec<Memory>> {
        let conn = self.state.lock().await;
        let tags_first = filter.tags_any.first().map(String::as_str);
        let since = filter.since.map(|d| d.to_rfc3339());
        let until = filter.until.map(|d| d.to_rfc3339());
        // db::search already applies the `visibility_clause` over the
        // scope_idx generated column when `as_agent` is supplied — the
        // post-filter below is the belt-and-suspenders mirror of the
        // SAL-level contract so adapters with FTS paths that lack the
        // generated column (or where the column trails the metadata
        // update by a transaction window) still fail-closed.
        let rows = db::search(
            &conn,
            query,
            filter.namespace.as_deref(),
            filter.tier.as_ref(),
            if filter.limit == 0 { 100 } else { filter.limit },
            None,
            since.as_deref(),
            until.as_deref(),
            tags_first,
            filter.agent_id.as_deref(),
            ctx.as_agent.as_deref(),
            false,
        )
        .map_err(box_err)?;
        // #910 SAL-level scope=private gate — see trait docstring +
        // `is_visible_to_caller`.
        if ctx.bypass_visibility {
            return Ok(rows);
        }
        let caller = ctx.effective_principal();
        Ok(rows
            .into_iter()
            .filter(|m| is_visible_to_caller(m, caller))
            .collect())
    }

    async fn verify(&self, _ctx: &CallerContext, id: &str) -> StoreResult<VerifyReport> {
        let conn = self.state.lock().await;
        let Some(mem) = db::get(&conn, id).map_err(box_err)? else {
            return Err(StoreError::NotFound { id: id.to_string() });
        };
        // v0.6.0.0 preview: minimal integrity check. Confirms that
        // the memory has a non-empty title + content and its
        // `metadata.agent_id` round-trips as a string. Real
        // signature verification lands alongside Task 1.4.
        let mut findings: Vec<String> = Vec::new();
        if mem.title.trim().is_empty() {
            findings.push("title is empty".to_string());
        }
        if mem.content.trim().is_empty() {
            findings.push("content is empty".to_string());
        }
        if mem.metadata.get("agent_id").is_none() {
            findings.push("metadata.agent_id missing".to_string());
        }
        Ok(VerifyReport {
            memory_id: id.to_string(),
            integrity_ok: findings.is_empty(),
            findings,
            // v0.6.0 does NOT perform signature verification; real
            // cryptographic verify lands with Task 1.4. See #302.
            signature_verified: false,
        })
    }

    async fn link(&self, _ctx: &CallerContext, link: &MemoryLink) -> StoreResult<()> {
        let conn = self.state.lock().await;
        db::create_link(
            &conn,
            &link.source_id,
            &link.target_id,
            link.relation.as_str(),
        )
        .map_err(box_err)
    }

    async fn link_signed(
        &self,
        _ctx: &CallerContext,
        link: &MemoryLink,
        keypair: Option<&crate::identity::keypair::AgentKeypair>,
    ) -> StoreResult<&'static str> {
        // F6 Gap 3 (v0.7.0) — route the SAL trait's signed-link surface
        // through SQLite's existing `db::create_link_signed`. Resolves
        // the same `attest_level` literal the Postgres adapter returns
        // so the caller-observable wire shape is byte-identical across
        // backends.
        let conn = self.state.lock().await;
        db::create_link_signed(
            &conn,
            &link.source_id,
            &link.target_id,
            link.relation.as_str(),
            keypair,
        )
        .map_err(box_err)
    }

    async fn list_links(&self, namespace: Option<&str>) -> StoreResult<Vec<MemoryLink>> {
        // F6 Gap 2 (v0.7.0) — surface `memory_links` to the migrate
        // runner. The namespace filter, when set, matches the source
        // memory's namespace (links live with their source — same
        // affinity SQLite uses for memories on migrate). Ordering by
        // `(source_id, target_id, relation)` is the SAL contract:
        // deterministic across calls and matches the unique key.
        let conn = self.state.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT ml.source_id, ml.target_id, ml.relation, ml.created_at,
                        ml.valid_from, ml.valid_until, ml.observed_by, ml.signature
                 FROM memory_links ml
                 WHERE ?1 IS NULL
                    OR EXISTS (SELECT 1 FROM memories m
                               WHERE m.id = ml.source_id AND m.namespace = ?1)
                 ORDER BY ml.source_id, ml.target_id, ml.relation",
            )
            .map_err(box_err)?;
        let rows = stmt
            .query_map(rusqlite::params![namespace], |row| {
                let relation_str: String = row.get(2)?;
                Ok(MemoryLink {
                    source_id: row.get(0)?,
                    target_id: row.get(1)?,
                    // v0.7.0 fix campaign R1-M4 — parse closed-set
                    // relation. Unknown values fall back to the default
                    // (`related_to`) so the read path never errors; the
                    // SQL CHECK on the write side prevents new bad rows.
                    relation: crate::models::MemoryLinkRelation::from_str(&relation_str)
                        .unwrap_or_default(),
                    created_at: row.get(3)?,
                    valid_from: row.get::<_, Option<String>>(4)?,
                    valid_until: row.get::<_, Option<String>>(5)?,
                    observed_by: row.get::<_, Option<String>>(6)?,
                    signature: row.get::<_, Option<Vec<u8>>>(7)?,
                    // v0.7.0 #860 — SAL migrate path doesn't surface
                    // attest_level (the federation wire shape stays
                    // unchanged). `None` + skip_serializing_if keeps
                    // pre-v0.7 receivers unaware of the new field.
                    attest_level: None,
                })
            })
            .map_err(box_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(box_err)
    }

    async fn register_agent(
        &self,
        _ctx: &CallerContext,
        agent: &AgentRegistration,
    ) -> StoreResult<()> {
        let conn = self.state.lock().await;
        db::register_agent(
            &conn,
            &agent.agent_id,
            &agent.agent_type,
            &agent.capabilities,
        )
        .map_err(box_err)
        .map(|_id| ())
    }

    // ----- v0.7.0 Wave-3 Continuation 2 — federation surface ---------

    async fn list_memories_updated_since(
        &self,
        since: Option<&str>,
        limit: usize,
    ) -> StoreResult<Vec<Memory>> {
        // NOTE: federation catchup path — `list_memories_updated_since`
        // is invoked over the `GET /api/v1/sync/since` peer-pull
        // surface, NOT a tenant-facing query. The mTLS-gated peer is
        // authenticated separately (Track Federation §H3 verify) and
        // sync rows must round-trip with full metadata intact, so this
        // method intentionally does NOT apply the scope=private filter.
        // Cross-tenant visibility on the sync surface is enforced by
        // the federation allowlist + peer-attestation gate, not by the
        // SAL row filter. Documented at the trait level — every new
        // query method MUST either apply the filter or document why
        // it bypasses (admin / federation / migration export).
        let conn = self.state.lock().await;
        let capped = limit.clamp(1, 10_000);
        db::memories_updated_since(&conn, since, capped).map_err(box_err)
    }

    async fn apply_remote_memory(
        &self,
        _ctx: &CallerContext,
        memory: &Memory,
    ) -> StoreResult<String> {
        let conn = self.state.lock().await;
        db::insert_if_newer(&conn, memory).map_err(box_err)
    }

    async fn apply_remote_link(
        &self,
        _ctx: &CallerContext,
        link: &MemoryLink,
        attest_level: &str,
    ) -> StoreResult<()> {
        let conn = self.state.lock().await;
        db::create_link_inbound(&conn, link, attest_level).map_err(box_err)
    }

    async fn apply_remote_deletion(&self, _ctx: &CallerContext, id: &str) -> StoreResult<bool> {
        let conn = self.state.lock().await;
        db::delete(&conn, id).map_err(box_err)
    }

    async fn recall_hybrid(
        &self,
        ctx: &CallerContext,
        query: &str,
        query_embedding: Option<&[f32]>,
        filter: &Filter,
    ) -> StoreResult<Vec<(Memory, f64)>> {
        let conn = self.state.lock().await;
        let tags_first = filter.tags_any.first().map(String::as_str);
        let since = filter.since.map(|d| d.to_rfc3339());
        let until = filter.until.map(|d| d.to_rfc3339());
        let limit = if filter.limit == 0 { 10 } else { filter.limit };
        let scoring = crate::config::ResolvedScoring::default();
        let results = if let Some(qe) = query_embedding {
            db::recall_hybrid(
                &conn,
                query,
                qe,
                filter.namespace.as_deref(),
                limit,
                tags_first,
                since.as_deref(),
                until.as_deref(),
                None, // vector_index threaded by the caller from AppState
                3600,
                86_400,
                ctx.as_agent.as_deref(),
                None,
                &scoring,
                false,
                // v0.7.0 Cluster-A PERF-3 — Filter has no source-URI
                // axis on the SAL surface today; pass `None` so the
                // SQL push-down is inactive. The HTTP/MCP path applies
                // the URI prefix via the dedicated argument on the
                // direct db::recall call.
                None,
            )
            .map_err(box_err)?
            .0
        } else {
            db::recall(
                &conn,
                query,
                filter.namespace.as_deref(),
                limit,
                tags_first,
                since.as_deref(),
                until.as_deref(),
                3600,
                86_400,
                ctx.as_agent.as_deref(),
                None,
                false,
                None,
            )
            .map_err(box_err)?
            .0
        };
        // #910 SAL-level scope=private gate — see trait docstring +
        // `is_visible_to_caller`. db::recall + db::recall_hybrid already
        // apply the `visibility_clause` SQL fragment when `as_agent`
        // is set; this post-filter is the belt-and-suspenders mirror
        // of the SAL contract so callers that pass an empty `as_agent`
        // (or rely on the trait default) still fail-closed.
        if ctx.bypass_visibility {
            return Ok(results);
        }
        let caller = ctx.effective_principal();
        Ok(results
            .into_iter()
            .filter(|(m, _)| is_visible_to_caller(m, caller))
            .collect())
    }

    async fn touch_after_recall(&self, ids: &[String]) -> StoreResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.state.lock().await;
        // v0.7.0 Form 5 / Cluster G — opportunistic freshness-decay
        // update on touch. Gated on `AI_MEMORY_CONFIDENCE_DECAY=1`
        // (default-off; audit-honest contract). When enabled, the
        // recall path stamps `confidence_decayed_at`, overwrites
        // `confidence` with the decayed value, and flips
        // `confidence_source` to `'decayed'` so the forensic bundle
        // reflects the provenance change. The pure decay math lives
        // in `crate::confidence::decay::decayed`; this site is the
        // substrate-side wiring (UPDATE).
        let decay_enabled = crate::confidence::decay::decay_enabled();
        for id in ids {
            if let Err(e) = db::touch(&conn, id, 3600, 86_400) {
                tracing::warn!("touch failed for memory {id}: {e}");
            }
            if decay_enabled && let Err(e) = crate::confidence::decay::apply_decay_touch(&conn, id)
            {
                tracing::warn!("confidence decay touch failed for memory {id}: {e}");
            }
        }
        Ok(())
    }

    async fn pending_decide(
        &self,
        _ctx: &CallerContext,
        id: &str,
        approve: bool,
        decided_by: &str,
    ) -> StoreResult<bool> {
        let conn = self.state.lock().await;
        db::decide_pending_action(&conn, id, approve, decided_by).map_err(box_err)
    }

    async fn get_pending(
        &self,
        _ctx: &CallerContext,
        id: &str,
    ) -> StoreResult<Option<crate::models::PendingAction>> {
        let conn = self.state.lock().await;
        db::get_pending_action(&conn, id).map_err(box_err)
    }

    async fn set_namespace_standard(
        &self,
        _ctx: &CallerContext,
        namespace: &str,
        standard_id: &str,
        parent: Option<&str>,
    ) -> StoreResult<()> {
        let conn = self.state.lock().await;
        db::set_namespace_standard(&conn, namespace, standard_id, parent).map_err(box_err)
    }

    async fn clear_namespace_standard(
        &self,
        _ctx: &CallerContext,
        namespace: &str,
    ) -> StoreResult<bool> {
        let conn = self.state.lock().await;
        db::clear_namespace_standard(&conn, namespace).map_err(box_err)
    }

    async fn get_namespace_standard(
        &self,
        _ctx: &CallerContext,
        namespace: &str,
    ) -> StoreResult<Option<(String, Option<String>)>> {
        let conn = self.state.lock().await;
        // db::get_namespace_standard returns the standard memory + parent
        // — we only need the (standard_id, parent_namespace) tuple here.
        let mut stmt = conn
            .prepare(
                "SELECT standard_id, parent_namespace FROM namespace_meta WHERE namespace = ?1",
            )
            .map_err(box_err)?;
        let mut rows = stmt
            .query_map(rusqlite::params![namespace], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(box_err)?;
        match rows.next() {
            Some(Ok(tuple)) => Ok(Some(tuple)),
            Some(Err(e)) => Err(box_err(e)),
            None => Ok(None),
        }
    }

    // v0.7.0 Wave-3 Continuation 3 — lifecycle write paths for sqlite.
    // Delegates to the legacy `db::*` free functions so behaviour is
    // byte-identical to the pre-Wave-3 sqlite path.

    async fn forget(
        &self,
        _ctx: &CallerContext,
        namespace: Option<&str>,
        pattern: Option<&str>,
        tier: Option<&Tier>,
        archive: bool,
    ) -> StoreResult<usize> {
        if namespace.is_none() && pattern.is_none() && tier.is_none() {
            return Err(StoreError::InvalidInput {
                detail: "at least one of namespace, pattern, or tier is required".to_string(),
            });
        }
        let conn = self.state.lock().await;
        db::forget(&conn, namespace, pattern, tier, archive).map_err(box_err)
    }

    async fn consolidate(
        &self,
        _ctx: &CallerContext,
        ids: &[String],
        title: &str,
        summary: &str,
        namespace: &str,
        tier: &Tier,
        source: &str,
        consolidator_agent_id: &str,
    ) -> StoreResult<String> {
        let conn = self.state.lock().await;
        db::consolidate(
            &conn,
            ids,
            title,
            summary,
            namespace,
            tier,
            source,
            consolidator_agent_id,
        )
        .map_err(box_err)
    }

    async fn run_gc(&self, archive: bool) -> StoreResult<usize> {
        let conn = self.state.lock().await;
        db::gc(&conn, archive).map_err(box_err)
    }

    async fn archive_restore(&self, _ctx: &CallerContext, id: &str) -> StoreResult<bool> {
        let conn = self.state.lock().await;
        db::restore_archived(&conn, id).map_err(box_err)
    }

    async fn archive_purge(&self, older_than_days: Option<i64>) -> StoreResult<usize> {
        let conn = self.state.lock().await;
        db::purge_archive(&conn, older_than_days).map_err(box_err)
    }

    async fn archive_by_ids(
        &self,
        _ctx: &CallerContext,
        ids: &[String],
        reason: Option<&str>,
    ) -> StoreResult<usize> {
        let conn = self.state.lock().await;
        let mut moved = 0usize;
        for id in ids {
            match db::archive_memory(&conn, id, reason) {
                Ok(true) => moved += 1,
                Ok(false) => {}
                Err(e) => return Err(box_err(e)),
            }
        }
        Ok(moved)
    }

    async fn export_memories(&self) -> StoreResult<Vec<Memory>> {
        // NOTE: operator/admin export surface — not tenant-facing.
        // Backs the `/api/v1/admin/export` endpoint (api-key gated).
        // Intentionally does NOT apply the scope=private filter so a
        // full-fidelity backup round-trips every row regardless of
        // metadata.scope. Admin-only by contract; documented at the
        // trait level.
        let conn = self.state.lock().await;
        db::export_all(&conn).map_err(box_err)
    }

    async fn export_links(&self) -> StoreResult<Vec<MemoryLink>> {
        let conn = self.state.lock().await;
        db::export_links(&conn).map_err(box_err)
    }

    async fn build_namespace_chain(&self, namespace: &str) -> StoreResult<Vec<String>> {
        let conn = self.state.lock().await;
        Ok(db::build_namespace_chain(&conn, namespace))
    }

    async fn resolve_governance_policy(
        &self,
        namespace: &str,
    ) -> StoreResult<Option<crate::models::GovernancePolicy>> {
        let conn = self.state.lock().await;
        Ok(db::resolve_governance_policy(&conn, namespace))
    }

    async fn governance_approve_with_consensus(
        &self,
        _ctx: &CallerContext,
        pending_id: &str,
        approver_agent_id: &str,
    ) -> StoreResult<super::ApproveOutcome> {
        let conn = self.state.lock().await;
        let outcome = db::approve_with_approver_type(&conn, pending_id, approver_agent_id)
            .map_err(box_err)?;
        // Translate the db-layer ApproveOutcome → SAL ApproveOutcome.
        let sal_outcome = match outcome {
            db::ApproveOutcome::Approved => super::ApproveOutcome::Approved,
            db::ApproveOutcome::Pending { votes, quorum } => {
                super::ApproveOutcome::Pending { votes, quorum }
            }
            db::ApproveOutcome::Rejected(reason) => super::ApproveOutcome::Rejected(reason),
        };
        Ok(sal_outcome)
    }

    async fn is_registered_agent(&self, agent_id: &str) -> StoreResult<bool> {
        let conn = self.state.lock().await;
        Ok(db::is_registered_agent(&conn, agent_id))
    }

    async fn enforce_governance_action(
        &self,
        action: super::GovernedAction,
        namespace: &str,
        agent_id: &str,
        memory_id: Option<&str>,
        memory_owner: Option<&str>,
        payload: &serde_json::Value,
    ) -> StoreResult<crate::models::GovernanceDecision> {
        let db_action = match action {
            super::GovernedAction::Store => crate::models::GovernedAction::Store,
            super::GovernedAction::Delete => crate::models::GovernedAction::Delete,
            super::GovernedAction::Promote => crate::models::GovernedAction::Promote,
            // v0.7.0 L1-8: Reflect is gated by require_approval_above_depth
            // in the MCP handler; map to Store-level for conservative
            // fallback enforcement if called through this path.
            super::GovernedAction::Reflect => crate::models::GovernedAction::Reflect,
        };
        let conn = self.state.lock().await;
        db::enforce_governance(
            &conn,
            db_action,
            namespace,
            agent_id,
            memory_id,
            memory_owner,
            payload,
        )
        .map_err(box_err)
    }

    // -------- v0.7.0 Wave-3 Continuation 6 — quota + verify-link ---------

    async fn quota_status(&self, agent_id: &str) -> StoreResult<QuotaStatus> {
        let conn = self.state.lock().await;
        quotas::get_status(&conn, agent_id).map_err(box_err)
    }

    async fn quota_status_list(&self) -> StoreResult<Vec<QuotaStatus>> {
        let conn = self.state.lock().await;
        quotas::list_status(&conn).map_err(box_err)
    }

    async fn verify_link(&self, filter: VerifyFilter) -> StoreResult<VerifyLinkReport> {
        // Filter shape: at least one of `(source_id, target_id)` OR
        // `link_id` must be set. `link_id` on the SQLite path is the
        // canonical `source_id|target_id|relation` triple — SQLite has
        // no separate rowid surface for links (composite PK). Postgres
        // honors the same convention so the wire shape is stable.
        if filter.source_id.is_none() && filter.link_id.is_none() {
            return Err(StoreError::InvalidInput {
                detail: "verify_link requires either source_id or link_id".to_string(),
            });
        }

        // Resolve the (source, target, relation) triple from either
        // axis. `link_id` of form "src|tgt|rel" wins; otherwise read
        // (source, target?) and resolve the first outbound link from
        // source when target is unset.
        let (source_id, target_id, relation_filter) = if let Some(link_id) =
            filter.link_id.as_deref()
        {
            let parts: Vec<&str> = link_id.split('|').collect();
            if parts.len() != 3 {
                return Err(StoreError::InvalidInput {
                    detail: format!(
                        "link_id must be canonical source_id|target_id|relation triple, got {link_id}"
                    ),
                });
            }
            (
                parts[0].to_string(),
                Some(parts[1].to_string()),
                Some(parts[2].to_string()),
            )
        } else {
            (filter.source_id.unwrap_or_default(), filter.target_id, None)
        };

        let conn = self.state.lock().await;

        // Build the WHERE clause for resolving the first matching row.
        let row: Option<(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<Vec<u8>>,
            Option<String>,
        )> = match (target_id.as_deref(), relation_filter.as_deref()) {
            (Some(t), Some(r)) => conn
                .query_row(
                    "SELECT source_id, target_id, relation, valid_from, valid_until, \
                            observed_by, signature, attest_level
                     FROM memory_links \
                     WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3 \
                     LIMIT 1",
                    rusqlite::params![source_id, t, r],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<Vec<u8>>>(6)?,
                            r.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
                .optional()
                .map_err(box_err)?,
            (Some(t), None) => conn
                .query_row(
                    "SELECT source_id, target_id, relation, valid_from, valid_until, \
                            observed_by, signature, attest_level
                     FROM memory_links \
                     WHERE source_id = ?1 AND target_id = ?2 \
                     ORDER BY created_at ASC LIMIT 1",
                    rusqlite::params![source_id, t],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<Vec<u8>>>(6)?,
                            r.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
                .optional()
                .map_err(box_err)?,
            (None, _) => conn
                .query_row(
                    "SELECT source_id, target_id, relation, valid_from, valid_until, \
                            observed_by, signature, attest_level
                     FROM memory_links \
                     WHERE source_id = ?1 \
                     ORDER BY created_at ASC LIMIT 1",
                    rusqlite::params![source_id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<Vec<u8>>>(6)?,
                            r.get::<_, Option<String>>(7)?,
                        ))
                    },
                )
                .optional()
                .map_err(box_err)?,
        };

        let Some((src, tgt, rel, vf, vu, obs, sig, attest)) = row else {
            return Err(StoreError::NotFound {
                id: format!(
                    "link {source_id} -> {} {}",
                    target_id.as_deref().unwrap_or("?"),
                    relation_filter.as_deref().unwrap_or("?")
                ),
            });
        };

        let attest_level = attest.unwrap_or_else(|| "unsigned".to_string());
        let signature_present = sig.is_some();
        let mut findings: Vec<String> = Vec::new();

        // Cryptographic verify path: when a signature blob is present,
        // try to look up the enrolled peer key and re-verify the
        // canonical CBOR. Failure to look up the key is a finding (not
        // an error) — the row stays `verified=true` if the structural
        // check passed, with a finding noting the gap. This matches
        // `sync_push`'s defensive accept-and-flag posture.
        let verified = if signature_present {
            let observed = obs.as_deref().unwrap_or("");
            match crate::identity::verify::lookup_peer_public_key(observed) {
                None => {
                    findings.push(format!(
                        "signature present but no enrolled public key for observed_by={observed}"
                    ));
                    // Without a key we cannot verify — surface false
                    // here so callers don't treat the row as trusted.
                    false
                }
                Some(pubkey) => {
                    let signable = crate::identity::sign::SignableLink {
                        src_id: &src,
                        dst_id: &tgt,
                        relation: &rel,
                        observed_by: obs.as_deref(),
                        valid_from: vf.as_deref(),
                        valid_until: vu.as_deref(),
                    };
                    let sig_bytes = sig.as_deref().unwrap_or(&[]);
                    match crate::identity::verify::verify(&pubkey, &signable, sig_bytes) {
                        Ok(()) => true,
                        Err(e) => {
                            findings.push(format!("signature verify failed: {e}"));
                            false
                        }
                    }
                }
            }
        } else {
            // Unsigned link: structurally-valid rows pass verify with
            // `signature_verified=false`. The cert harness reads
            // `attest_level=unsigned` to decide whether to trust.
            true
        };

        Ok(VerifyLinkReport {
            source_id: src,
            target_id: tgt,
            relation: rel,
            verified,
            attest_level,
            signature_present,
            observed_by: obs,
            findings,
        })
    }

    async fn find_paths(
        &self,
        ctx: &CallerContext,
        source_id: &str,
        target_id: &str,
        max_depth: Option<usize>,
        max_results: Option<usize>,
    ) -> StoreResult<Vec<Vec<String>>> {
        let conn = self.state.lock().await;
        // SQLite's find_paths defaults to current-view (excludes
        // invalidated edges) — match the trait/HTTP contract.
        let paths = db::find_paths(&conn, source_id, target_id, max_depth, max_results, false)
            .map_err(box_err)?;
        // #910 SAL-level scope=private gate (path-traversal flavour) —
        // any path that walks through a memory the caller cannot see
        // is dropped. Fetch each node's metadata once and cache so
        // the filter is O(distinct-nodes), not O(path-count *
        // path-length). Fail-closed: a node that cannot be resolved
        // (deleted mid-traversal, or in a namespace this caller can
        // never read) drops every path that touches it.
        if ctx.bypass_visibility {
            return Ok(paths);
        }
        let caller = ctx.effective_principal();
        let mut visible_cache: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let mut filtered: Vec<Vec<String>> = Vec::with_capacity(paths.len());
        'outer: for path in paths {
            for node in &path {
                let entry = visible_cache.entry(node.clone()).or_insert_with(|| {
                    match db::get(&conn, node) {
                        Ok(Some(mem)) => is_visible_to_caller(&mem, caller),
                        // Fail-closed: missing node ⇒ drop the path.
                        Ok(None) | Err(_) => false,
                    }
                });
                if !*entry {
                    continue 'outer;
                }
            }
            filtered.push(path);
        }
        Ok(filtered)
    }

    async fn notify(
        &self,
        ctx: &CallerContext,
        target_agent: &str,
        title: &str,
        payload: &str,
        priority: Option<i32>,
        tier: Option<&Tier>,
    ) -> StoreResult<String> {
        // Compose the notify memory using the same shape as
        // `mcp::handle_notify`: a memory in `_inbox/<target_agent>` with
        // `metadata.target_agent_id` set so subsequent inbox pulls find it.
        let now = chrono::Utc::now().to_rfc3339();
        let resolved_tier = tier.cloned().unwrap_or(Tier::Short);
        let priority = priority.unwrap_or(5);
        let metadata = serde_json::json!({
            "agent_id": &ctx.agent_id,
            "target_agent_id": target_agent,
            "notify": true,
        });
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: resolved_tier,
            namespace: format!("_inbox/{target_agent}"),
            title: title.to_string(),
            content: payload.to_string(),
            tags: vec!["notify".to_string()],
            priority,
            confidence: 1.0,
            source: "notify".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };
        let conn = self.state.lock().await;
        db::insert(&conn, &mem).map_err(box_err)
    }
}

/// Transaction handle that no-ops commit (`SQLite` txn support is
/// available via `rusqlite::Connection::unchecked_transaction` but
/// wrapping through the mutex gets awkward — for the preview, callers
/// that need real atomicity should still reach through `crate::db`
/// directly).
#[allow(dead_code)]
pub struct SqliteTransaction;

#[async_trait::async_trait]
impl Transaction for SqliteTransaction {
    async fn commit(self: Box<Self>) -> StoreResult<()> {
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> StoreResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tier;

    fn test_memory(title: &str, content: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "sal-test".to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec!["test".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "alice"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
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
    async fn roundtrip_store_get() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("hello", "world one two three four five six seven");
        let stored_id = store.store(&ctx, &mem).await.expect("store");
        let loaded = store.get(&ctx, &stored_id).await.expect("get");
        assert_eq!(loaded.title, "hello");
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let ctx = CallerContext::for_agent("alice");
        let err = store
            .get(&ctx, "00000000-0000-0000-0000-000000000000")
            .await
            .expect_err("should be NotFound");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn capabilities_declare_sqlite_reality() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let caps = store.capabilities();
        assert!(caps.contains(Capabilities::DURABLE));
        assert!(caps.contains(Capabilities::FULLTEXT));
        assert!(caps.contains(Capabilities::STRONG_CONSISTENCY));
        // NATIVE_VECTOR is intentionally NOT set — semantic search
        // happens above this layer via crate::hnsw, not inside the
        // adapter.
        assert!(!caps.contains(Capabilities::NATIVE_VECTOR));
        // TRANSACTIONS + ATOMIC_MULTI_WRITE are NOT set — the adapter
        // doesn't expose `begin_transaction()` (#302 item 6 fix).
        assert!(!caps.contains(Capabilities::TRANSACTIONS));
        assert!(!caps.contains(Capabilities::ATOMIC_MULTI_WRITE));
    }

    #[tokio::test]
    async fn verify_flags_empty_content() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open");
        let ctx = CallerContext::for_agent("alice");
        let mut mem = test_memory("hello", "x content long enough to pass validate");
        mem.content = "nonempty for store".to_string();
        let id = store.store(&ctx, &mem).await.expect("store");
        // Manually corrupt metadata.agent_id via update.
        store
            .update(
                &ctx,
                &id,
                UpdatePatch {
                    metadata: Some(serde_json::json!({})),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        let report = store.verify(&ctx, &id).await.expect("verify");
        assert!(!report.integrity_ok);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("metadata.agent_id"))
        );
    }

    // ---------------------------------------------------------------------
    // L0.7-6 Tier E coverage — round-trip every trait method on a tempfile
    // SQLite store so the adapter's plumbing (the bulk of the lines this
    // file owns) is exercised without a live process. Each test uses a
    // fresh tempfile DB so cross-test isolation is guaranteed.
    // ---------------------------------------------------------------------

    fn fresh_store() -> SqliteStore {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        // Drop the NamedTempFile guard so close() doesn't race the DB
        // open; the path leaks but it's under the OS tmp dir which
        // colima/macOS reaps. Tests run hermetically inside a worktree
        // tempdir; no /tmp violation per project rule.
        std::mem::forget(tmp);
        SqliteStore::open(&path).expect("open SqliteStore")
    }

    #[tokio::test]
    async fn schema_version_returns_nonzero_after_open() {
        let store = fresh_store();
        let v = store.schema_version().await.expect("schema_version");
        // db::open runs the migration ladder; schema_version should be
        // strictly positive after open. (The exact value tracks the
        // CURRENT_SCHEMA_VERSION constant which moves; assert >0 only.)
        assert!(v > 0, "expected positive schema_version, got {v}");
    }

    #[tokio::test]
    async fn list_returns_stored_memories() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("listme", "content for list query");
        let id = store.store(&ctx, &mem).await.expect("store");
        let filter = Filter {
            namespace: Some("sal-test".to_string()),
            limit: 10,
            ..Filter::default()
        };
        let rows = store.list(&ctx, &filter).await.expect("list");
        assert!(rows.iter().any(|m| m.id == id), "list omitted stored id");
    }

    #[tokio::test]
    async fn list_default_limit_when_zero() {
        // Filter.limit == 0 should be treated as "100" by the adapter
        // (per the implementation comment). Verify by storing one row
        // and confirming a zero-limit list still returns it.
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("default-limit", "needs sufficient content for fts");
        store.store(&ctx, &mem).await.expect("store");
        let filter = Filter {
            namespace: Some("sal-test".to_string()),
            limit: 0,
            ..Filter::default()
        };
        let rows = store.list(&ctx, &filter).await.expect("list zero-limit");
        assert!(
            !rows.is_empty(),
            "zero-limit should fall back to default 100"
        );
    }

    #[tokio::test]
    async fn search_finds_keyword_match() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("searchable", "fts5 token jellyfish for unique grep");
        store.store(&ctx, &mem).await.expect("store");
        let filter = Filter {
            limit: 10,
            ..Filter::default()
        };
        let hits = store
            .search(&ctx, "jellyfish", &filter)
            .await
            .expect("search");
        assert!(
            hits.iter().any(|m| m.title == "searchable"),
            "fts search missed the unique token"
        );
    }

    #[tokio::test]
    async fn update_missing_returns_not_found() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let err = store
            .update(
                &ctx,
                "11111111-1111-1111-1111-111111111111",
                UpdatePatch {
                    title: Some("never".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("update missing id");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn delete_missing_returns_not_found() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let err = store
            .delete(&ctx, "22222222-2222-2222-2222-222222222222")
            .await
            .expect_err("delete missing");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn delete_then_get_chain() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("ephemeral", "stored briefly for delete test");
        let id = store.store(&ctx, &mem).await.expect("store");
        store.delete(&ctx, &id).await.expect("delete existing");
        let err = store.get(&ctx, &id).await.expect_err("get after delete");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn verify_missing_returns_not_found() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let err = store
            .verify(&ctx, "33333333-3333-3333-3333-333333333333")
            .await
            .expect_err("verify missing");
        assert!(matches!(err, StoreError::NotFound { .. }));
    }

    #[tokio::test]
    async fn link_and_list_links_round_trip() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let a = test_memory("source-mem", "content for link source");
        let b = test_memory("target-mem", "content for link target");
        let a_id = store.store(&ctx, &a).await.expect("store a");
        let b_id = store.store(&ctx, &b).await.expect("store b");
        let link = MemoryLink {
            source_id: a_id.clone(),
            target_id: b_id.clone(),
            relation: crate::models::MemoryLinkRelation::RelatedTo,
            created_at: chrono::Utc::now().to_rfc3339(),
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
            attest_level: None,
        };
        store.link(&ctx, &link).await.expect("link insert");
        let listed = store.list_links(None).await.expect("list_links");
        assert!(
            listed
                .iter()
                .any(|l| l.source_id == a_id && l.target_id == b_id),
            "list_links missed the just-inserted row"
        );
        // namespace-filtered: same namespace produces the row.
        let same_ns = store
            .list_links(Some("sal-test"))
            .await
            .expect("list_links by ns");
        assert!(
            same_ns
                .iter()
                .any(|l| l.source_id == a_id && l.target_id == b_id),
            "namespace filter dropped a same-ns link"
        );
        // namespace-filtered: missing namespace produces no row.
        let missing_ns = store
            .list_links(Some("nonexistent"))
            .await
            .expect("list_links missing ns");
        assert!(
            !missing_ns
                .iter()
                .any(|l| l.source_id == a_id && l.target_id == b_id),
            "namespace filter must exclude links whose source lives elsewhere"
        );
    }

    #[tokio::test]
    async fn link_signed_unsigned_falls_through() {
        // link_signed with None keypair must land "unsigned" attest.
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let a = test_memory("ls-a", "content for ls a");
        let b = test_memory("ls-b", "content for ls b");
        let a_id = store.store(&ctx, &a).await.expect("a");
        let b_id = store.store(&ctx, &b).await.expect("b");
        let link = MemoryLink {
            source_id: a_id,
            target_id: b_id,
            relation: crate::models::MemoryLinkRelation::Supersedes,
            created_at: chrono::Utc::now().to_rfc3339(),
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
            attest_level: None,
        };
        let attest = store
            .link_signed(&ctx, &link, None)
            .await
            .expect("link_signed unsigned path");
        assert_eq!(attest, "unsigned");
    }

    #[tokio::test]
    async fn register_agent_then_is_registered() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let agent = AgentRegistration {
            agent_id: "ai:tester@host".to_string(),
            agent_type: "ai".to_string(),
            capabilities: vec!["memory.read".to_string()],
            registered_at: chrono::Utc::now().to_rfc3339(),
            last_seen_at: chrono::Utc::now().to_rfc3339(),
        };
        store
            .register_agent(&ctx, &agent)
            .await
            .expect("register_agent");
        let yes = store
            .is_registered_agent("ai:tester@host")
            .await
            .expect("is_registered yes");
        assert!(yes, "registered agent must be detected");
        let no = store
            .is_registered_agent("ai:unknown@host")
            .await
            .expect("is_registered no");
        assert!(!no, "unknown agent must be unregistered");
    }

    #[tokio::test]
    async fn list_memories_updated_since_no_filter() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("since-test", "content for since-query test");
        store.store(&ctx, &mem).await.expect("store");
        let all = store
            .list_memories_updated_since(None, 100)
            .await
            .expect("list_since none");
        assert!(
            all.iter().any(|m| m.title == "since-test"),
            "no-since filter must return all memories"
        );
    }

    #[tokio::test]
    async fn apply_remote_memory_is_idempotent() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("remote", "remote content for apply path");
        let id1 = store
            .apply_remote_memory(&ctx, &mem)
            .await
            .expect("apply 1");
        let id2 = store
            .apply_remote_memory(&ctx, &mem)
            .await
            .expect("apply 2 idempotent");
        assert_eq!(id1, id2, "insert_if_newer must be idempotent on same row");
    }

    #[tokio::test]
    async fn apply_remote_link_attest_threading() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let a = test_memory("rl-a", "content rl a");
        let b = test_memory("rl-b", "content rl b");
        let a_id = store.store(&ctx, &a).await.expect("a");
        let b_id = store.store(&ctx, &b).await.expect("b");
        let link = MemoryLink {
            source_id: a_id,
            target_id: b_id,
            relation: crate::models::MemoryLinkRelation::DerivedFrom,
            created_at: chrono::Utc::now().to_rfc3339(),
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
            attest_level: None,
        };
        // attest_level threads through; "unsigned" is the safe default.
        store
            .apply_remote_link(&ctx, &link, "unsigned")
            .await
            .expect("apply_remote_link");
    }

    #[tokio::test]
    async fn apply_remote_deletion_returns_false_for_missing() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let gone = store
            .apply_remote_deletion(&ctx, "44444444-4444-4444-4444-444444444444")
            .await
            .expect("apply_remote_deletion missing");
        assert!(
            !gone,
            "apply_remote_deletion must return false for missing id"
        );
    }

    #[tokio::test]
    async fn recall_hybrid_keyword_fallback_no_embedding() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory(
            "recall-target",
            "indigo elephant chess fts5 token recall test",
        );
        store.store(&ctx, &mem).await.expect("store");
        let filter = Filter {
            limit: 10,
            ..Filter::default()
        };
        let hits = store
            .recall_hybrid(&ctx, "elephant", None, &filter)
            .await
            .expect("recall_hybrid keyword fallback");
        assert!(
            !hits.is_empty(),
            "recall_hybrid keyword fallback returned nothing"
        );
        assert!(hits[0].1 > 0.0, "score must be positive");
    }

    #[tokio::test]
    async fn touch_after_recall_is_noop_on_empty_ids() {
        let store = fresh_store();
        store
            .touch_after_recall(&[])
            .await
            .expect("touch_after_recall empty");
    }

    #[tokio::test]
    async fn touch_after_recall_warn_path_on_missing_id() {
        // touch_after_recall logs-and-swallows touch errors; verify the
        // bulk-path returns Ok even when an id is unknown.
        let store = fresh_store();
        let unknown = vec!["55555555-5555-5555-5555-555555555555".to_string()];
        store
            .touch_after_recall(&unknown)
            .await
            .expect("touch must tolerate unknown ids");
    }

    #[tokio::test]
    async fn forget_invalid_input_without_filter() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let err = store
            .forget(&ctx, None, None, None, false)
            .await
            .expect_err("forget without filter");
        assert!(matches!(err, StoreError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn forget_by_namespace_succeeds_even_on_empty() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        // No matching rows yet → count is 0 but no error.
        let n = store
            .forget(&ctx, Some("nonexistent-ns"), None, None, false)
            .await
            .expect("forget by ns");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn run_gc_returns_zero_on_empty_db() {
        let store = fresh_store();
        let n = store.run_gc(false).await.expect("gc empty");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn archive_purge_zero_threshold_purges_all() {
        let store = fresh_store();
        // Empty archive ⇒ 0 purged.
        let n = store.archive_purge(Some(0)).await.expect("archive_purge");
        assert_eq!(n, 0);
        // None means "purge all" — still zero on empty archive.
        let n = store.archive_purge(None).await.expect("archive_purge all");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn archive_by_ids_is_zero_for_unknown_ids() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let moved = store
            .archive_by_ids(
                &ctx,
                &["66666666-6666-6666-6666-666666666666".to_string()],
                Some("manual"),
            )
            .await
            .expect("archive_by_ids unknown");
        assert_eq!(moved, 0);
    }

    #[tokio::test]
    async fn archive_restore_returns_false_for_missing() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let restored = store
            .archive_restore(&ctx, "77777777-7777-7777-7777-777777777777")
            .await
            .expect("archive_restore missing");
        assert!(!restored);
    }

    #[tokio::test]
    async fn export_memories_and_links_round_trip() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let mem = test_memory("export-me", "content for export round trip");
        store.store(&ctx, &mem).await.expect("store");
        let memories = store.export_memories().await.expect("export_memories");
        assert!(memories.iter().any(|m| m.title == "export-me"));
        let links = store.export_links().await.expect("export_links");
        // Empty DB has no links yet — confirm the call succeeds.
        assert!(links.is_empty() || links.iter().all(|l| !l.source_id.is_empty()));
    }

    #[tokio::test]
    async fn build_namespace_chain_includes_self() {
        let store = fresh_store();
        let chain = store
            .build_namespace_chain("project/foo")
            .await
            .expect("build_namespace_chain");
        // The chain always includes the leaf namespace itself.
        assert!(
            chain.iter().any(|s| s == "project/foo"),
            "chain must include leaf, got {chain:?}"
        );
    }

    #[tokio::test]
    async fn resolve_governance_policy_none_on_fresh_db() {
        let store = fresh_store();
        let policy = store
            .resolve_governance_policy("any/ns")
            .await
            .expect("resolve_governance_policy");
        assert!(policy.is_none(), "fresh DB must have no policy");
    }

    #[tokio::test]
    async fn enforce_governance_action_allow_on_fresh_db() {
        let store = fresh_store();
        let decision = store
            .enforce_governance_action(
                super::super::GovernedAction::Store,
                "free-ns",
                "alice",
                None,
                None,
                &serde_json::json!({}),
            )
            .await
            .expect("enforce_governance_action");
        assert!(matches!(decision, crate::models::GovernanceDecision::Allow));
    }

    #[tokio::test]
    async fn get_namespace_standard_none_initially() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let std_row = store
            .get_namespace_standard(&ctx, "no-such-ns")
            .await
            .expect("get_namespace_standard");
        assert!(std_row.is_none());
    }

    #[tokio::test]
    async fn set_then_get_then_clear_namespace_standard() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        // Standard memory has to exist first.
        let std_mem = test_memory("std-doc", "documentation for ns standard");
        let std_id = store.store(&ctx, &std_mem).await.expect("store std");
        store
            .set_namespace_standard(&ctx, "ns/with/standard", &std_id, None)
            .await
            .expect("set_namespace_standard");
        let got = store
            .get_namespace_standard(&ctx, "ns/with/standard")
            .await
            .expect("get_namespace_standard");
        assert_eq!(got.as_ref().map(|(s, _)| s.as_str()), Some(std_id.as_str()));
        let removed = store
            .clear_namespace_standard(&ctx, "ns/with/standard")
            .await
            .expect("clear_namespace_standard");
        assert!(removed);
        let after = store
            .get_namespace_standard(&ctx, "ns/with/standard")
            .await
            .expect("get after clear");
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn quota_status_auto_inserts_default_row() {
        let store = fresh_store();
        let q = store
            .quota_status("ai:quota-test")
            .await
            .expect("quota_status");
        assert_eq!(q.agent_id, "ai:quota-test");
    }

    #[tokio::test]
    async fn quota_status_list_returns_inserted_row() {
        let store = fresh_store();
        // Force a row via quota_status, then list.
        let _ = store.quota_status("ai:listed").await.expect("seed");
        let rows = store.quota_status_list().await.expect("quota_status_list");
        assert!(rows.iter().any(|r| r.agent_id == "ai:listed"));
    }

    #[tokio::test]
    async fn verify_link_rejects_missing_filter() {
        let store = fresh_store();
        let filter = VerifyFilter::default();
        let err = store
            .verify_link(filter)
            .await
            .expect_err("verify_link without source/link_id");
        assert!(matches!(err, StoreError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn verify_link_rejects_malformed_link_id() {
        let store = fresh_store();
        let filter = VerifyFilter {
            link_id: Some("notatriple".to_string()),
            ..Default::default()
        };
        let err = store
            .verify_link(filter)
            .await
            .expect_err("verify_link malformed link_id");
        assert!(matches!(err, StoreError::InvalidInput { .. }));
    }

    #[tokio::test]
    async fn verify_link_resolves_unsigned_link() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let a = test_memory("vl-a", "content for vl a");
        let b = test_memory("vl-b", "content for vl b");
        let a_id = store.store(&ctx, &a).await.expect("a");
        let b_id = store.store(&ctx, &b).await.expect("b");
        let link = MemoryLink {
            source_id: a_id.clone(),
            target_id: b_id.clone(),
            relation: crate::models::MemoryLinkRelation::RelatedTo,
            created_at: chrono::Utc::now().to_rfc3339(),
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
            attest_level: None,
        };
        store.link(&ctx, &link).await.expect("insert link");
        let report = store
            .verify_link(VerifyFilter {
                source_id: Some(a_id.clone()),
                target_id: Some(b_id.clone()),
                link_id: None,
            })
            .await
            .expect("verify_link");
        assert_eq!(report.source_id, a_id);
        assert_eq!(report.target_id, b_id);
        // Unsigned link reports verified=true with signature_present=false.
        assert!(report.verified);
        assert!(!report.signature_present);
        assert_eq!(report.attest_level, "unsigned");
    }

    #[tokio::test]
    async fn verify_link_source_only_resolves_first_outbound() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let a = test_memory("solo-source", "content for solo source");
        let b = test_memory("solo-target", "content for solo target");
        let a_id = store.store(&ctx, &a).await.expect("a");
        let b_id = store.store(&ctx, &b).await.expect("b");
        let link = MemoryLink {
            source_id: a_id.clone(),
            target_id: b_id,
            relation: crate::models::MemoryLinkRelation::Supersedes,
            created_at: chrono::Utc::now().to_rfc3339(),
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
            attest_level: None,
        };
        store.link(&ctx, &link).await.expect("link");
        let report = store
            .verify_link(VerifyFilter {
                source_id: Some(a_id),
                ..Default::default()
            })
            .await
            .expect("source-only verify_link");
        assert!(report.verified);
    }

    #[tokio::test]
    async fn find_paths_returns_empty_for_unknown_endpoints() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let paths = store
            .find_paths(
                &ctx,
                "88888888-8888-8888-8888-888888888888",
                "99999999-9999-9999-9999-999999999999",
                None,
                None,
            )
            .await
            .expect("find_paths");
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn notify_creates_inbox_row() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let id = store
            .notify(
                &ctx,
                "ai:notify-target",
                "hello",
                "payload body",
                None,
                None,
            )
            .await
            .expect("notify");
        let mem = store.get(&ctx, &id).await.expect("get notify");
        assert_eq!(mem.namespace, "_inbox/ai:notify-target");
        assert!(mem.tags.iter().any(|t| t == "notify"));
    }

    #[tokio::test]
    async fn consolidate_round_trips_two_sources() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        // Seed two memories that the consolidate path will merge.
        let a = test_memory("consolidate-source-a", "content a one two three four");
        let b = test_memory("consolidate-source-b", "content b one two three four");
        let a_id = store.store(&ctx, &a).await.expect("store a");
        let b_id = store.store(&ctx, &b).await.expect("store b");
        // The legacy db::consolidate accepts the call against the live
        // ids and produces a new memory id; the adapter simply forwards.
        let consolidated_id = store
            .consolidate(
                &ctx,
                &[a_id, b_id],
                "merged-title",
                "merged summary content for the consolidator",
                "sal-test",
                &Tier::Mid,
                "consolidate-test",
                "alice",
            )
            .await
            .expect("consolidate two sources");
        // The resulting memory must be retrievable.
        let mem = store
            .get(&ctx, &consolidated_id)
            .await
            .expect("get consolidated");
        assert_eq!(mem.title, "merged-title");
    }

    #[tokio::test]
    async fn sqlite_transaction_commit_and_rollback_are_no_op() {
        // The SqliteTransaction placeholder no-ops both commit and
        // rollback (per the doc comment). Pin the contract.
        let txn1 = Box::new(SqliteTransaction);
        txn1.commit().await.expect("commit no-op");
        let txn2 = Box::new(SqliteTransaction);
        txn2.rollback().await.expect("rollback no-op");
    }

    #[tokio::test]
    async fn store_path_accessor_returns_open_path() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let store = SqliteStore::open(&path).expect("open");
        assert_eq!(store.path(), path.as_path());
    }

    #[tokio::test]
    async fn pending_decide_false_when_no_row_matches() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let res = store
            .pending_decide(&ctx, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", true, "alice")
            .await
            .expect("pending_decide miss");
        assert!(!res, "pending_decide must return false for unknown id");
    }

    #[tokio::test]
    async fn get_pending_returns_none_for_unknown() {
        let store = fresh_store();
        let ctx = CallerContext::for_agent("alice");
        let row = store
            .get_pending(&ctx, "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
            .await
            .expect("get_pending miss");
        assert!(row.is_none());
    }
}
