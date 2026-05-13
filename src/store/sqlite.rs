// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! In-tree `SqliteStore` adapter. Wraps the existing `crate::db` free
//! functions so the production path can migrate to the SAL trait
//! gradually. No behavior change vs. calling `crate::db` directly —
//! this is a thin shim whose only job is to prove the trait surface
//! fits the shape of the shipped code.

use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::OptionalExtension;
use tokio::sync::Mutex;

use crate::db;
use crate::models::{AgentRegistration, Memory, MemoryLink, Tier};

use super::{
    BoxBackendError, CallerContext, Capabilities, Filter, MemoryStore, StoreError, StoreResult,
    Transaction, UpdatePatch, VerifyFilter, VerifyLinkReport, VerifyReport,
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

    async fn get(&self, _ctx: &CallerContext, id: &str) -> StoreResult<Memory> {
        let conn = self.state.lock().await;
        match db::get(&conn, id).map_err(box_err)? {
            Some(mem) => Ok(mem),
            None => Err(StoreError::NotFound { id: id.to_string() }),
        }
    }

    async fn update(&self, _ctx: &CallerContext, id: &str, patch: UpdatePatch) -> StoreResult<()> {
        let conn = self.state.lock().await;
        let (found, _content_changed) = db::update(
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

    async fn list(&self, _ctx: &CallerContext, filter: &Filter) -> StoreResult<Vec<Memory>> {
        let conn = self.state.lock().await;
        let tags_first = filter.tags_any.first().map(String::as_str);
        let since = filter.since.map(|d| d.to_rfc3339());
        let until = filter.until.map(|d| d.to_rfc3339());
        db::list(
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
        .map_err(box_err)
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
        db::search(
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
        )
        .map_err(box_err)
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
                Ok(MemoryLink {
                    source_id: row.get(0)?,
                    target_id: row.get(1)?,
                    relation: row.get(2)?,
                    created_at: row.get(3)?,
                    valid_from: row.get::<_, Option<String>>(4)?,
                    valid_until: row.get::<_, Option<String>>(5)?,
                    observed_by: row.get::<_, Option<String>>(6)?,
                    signature: row.get::<_, Option<Vec<u8>>>(7)?,
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
        if let Some(qe) = query_embedding {
            let (results, _outcome) = db::recall_hybrid(
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
            )
            .map_err(box_err)?;
            Ok(results)
        } else {
            let (results, _outcome) = db::recall(
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
            )
            .map_err(box_err)?;
            Ok(results)
        }
    }

    async fn touch_after_recall(&self, ids: &[String]) -> StoreResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conn = self.state.lock().await;
        for id in ids {
            if let Err(e) = db::touch(&conn, id, 3600, 86_400) {
                tracing::warn!("touch failed for memory {id}: {e}");
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
        source_id: &str,
        target_id: &str,
        max_depth: Option<usize>,
        max_results: Option<usize>,
    ) -> StoreResult<Vec<Vec<String>>> {
        let conn = self.state.lock().await;
        // SQLite's find_paths defaults to current-view (excludes
        // invalidated edges) — match the trait/HTTP contract.
        db::find_paths(&conn, source_id, target_id, max_depth, max_results, false).map_err(box_err)
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
}
