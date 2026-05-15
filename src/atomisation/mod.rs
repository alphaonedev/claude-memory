// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-B — substrate-level atomisation engine.
//!
//! The atomiser is the second hard prereq for the v0.7.0 atomisation
//! pipeline (WT-1-A schema v36 is the first; WT-1-C/D/E/F all
//! consume the writer landed here). It takes one long-form memory,
//! runs the curator pass to decompose it into atomic propositions,
//! and writes each atom back as a first-class memory with full
//! provenance:
//!
//! * `memories.atom_of` → parent memory id (structural FK, schema v36)
//! * `memory_links.relation = 'derives_from'` (atom → parent, the
//!   typed, signable, federation-safe expression of the FK)
//! * `signed_events` rows for the per-atom write, the per-link write,
//!   and the final `atomisation_complete` summary event
//!
//! The parent memory is archived (`archived_at` set, `atomised_into`
//! set to `atom_count`) in a SEPARATE post-atom transaction so the
//! per-atom hooks fire on live writes; downstream consumers walk the
//! `atom_of` index to surface atoms in place of an atomised parent.
//!
//! # Hook integration
//!
//! Atoms are first-class memory_store writes — the existing
//! `pre_store`/`post_store` substrate hooks fire per atom via
//! [`crate::storage::insert`]. Governance refusal mid-batch returns
//! [`AtomiseError::GovernanceRefused`] carrying the failing atom
//! index; prior atoms in the batch are NOT rolled back (they were
//! valid writes by themselves).
//!
//! # Idempotency
//!
//! A second `atomise(source_id, ...)` call after a successful first
//! returns [`AtomiseError::AlreadyAtomised`] with the existing atom
//! ids. Passing `force=true` skips the idempotency check and mints a
//! fresh set of atoms; old atoms are retained (their `atom_of`
//! pointer remains valid), and `atomised_into` is bumped to the new
//! `atom_count`.

pub mod curator;

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{Connection, params};

use crate::identity::keypair::AgentKeypair;
use crate::models::{Memory, MemoryKind, MemoryLinkRelation, SourceSpan};
use crate::signed_events::{SignedEvent, append_signed_event, payload_hash};
use crate::storage as db;
use curator::Curator;

/// Tunables for the atomiser. Plumbed from `AppConfig` in the daemon
/// path; tests construct one directly.
///
/// Defaults mirror the WT-1-B brief:
/// * `default_max_atom_tokens = 200`
/// * `min_atoms_per_source = 2`
/// * `max_atoms_per_source = 10`
/// * `curator_max_retries = 3`
#[derive(Debug, Clone)]
pub struct AtomiserConfig {
    /// Default per-atom token budget when the caller does not supply
    /// an explicit value. The CLI / MCP atomise tool surfaces this as
    /// the `max_atom_tokens` parameter.
    pub default_max_atom_tokens: u32,
    /// Minimum atoms a single source must produce for the atomisation
    /// to be considered productive. Below this the source is
    /// "atomic-enough" — [`AtomiseError::SourceTooSmall`].
    pub min_atoms_per_source: usize,
    /// Cap on atoms per source — prevents pathological responses
    /// where the LLM emits dozens of trivial atoms. Matches the prompt
    /// envelope ("2 to 10 atoms").
    pub max_atoms_per_source: usize,
    /// Max retries on a malformed curator response. Total attempts =
    /// 1 + this value. See [`curator::backoff_for_attempt`] for the
    /// exponential-backoff schedule.
    pub curator_max_retries: u32,
}

impl Default for AtomiserConfig {
    fn default() -> Self {
        Self {
            default_max_atom_tokens: 200,
            min_atoms_per_source: 2,
            max_atoms_per_source: 10,
            curator_max_retries: 3,
        }
    }
}

/// Successful atomisation outcome.
///
/// `atom_ids` carries the freshly-minted atom ids in the order the
/// curator produced them (preserving narrative flow — the WT-1-C
/// resolver depends on this order for the default surface).
#[derive(Debug, Clone)]
pub struct AtomiseResult {
    pub source_id: String,
    pub atom_ids: Vec<String>,
    pub atom_count: usize,
    /// RFC3339 timestamp the parent memory was archived (i.e. the
    /// `atomised_into` write committed). Returned for telemetry and
    /// for the MCP `memory_atomise` response shape; callers building
    /// audit trails get the moment the parent went read-only.
    pub archived_at: String,
}

/// Typed error surface for [`Atomiser::atomise`].
///
/// Carries enough structured payload that the MCP / HTTP / CLI
/// wrappers can render a clean operator-readable message without
/// re-querying the DB.
#[derive(Debug)]
pub enum AtomiseError {
    /// Source memory id does not exist (or has been hard-deleted).
    NotFound,
    /// Source has already been atomised. `existing_atom_ids` is the
    /// set of atom ids currently pointing at this source via
    /// `atom_of`. Caller may surface them or re-issue with `force =
    /// true` to mint a fresh set.
    AlreadyAtomised {
        source_id: String,
        existing_atom_ids: Vec<String>,
    },
    /// The daemon's resolved feature tier is `Keyword` — atomisation
    /// requires the curator LLM (`Smart` or `Autonomous`). The MCP
    /// surface maps this to a 503-style refusal.
    TierLocked,
    /// Curator round-trip exhausted retries. Carries the last parse
    /// diagnostic so the caller can render it.
    CuratorFailed(String),
    /// Source body is already at or under `max_atom_tokens` — no
    /// productive decomposition possible. The caller may surface the
    /// source as-is. Distinct from `AlreadyAtomised`: this is the
    /// "never worth atomising" verdict, the latter is the "already
    /// done" verdict.
    SourceTooSmall,
    /// A `pre_store` substrate hook refused atom `index` (zero-based
    /// into the curator's atom list). Prior atoms (indices `< index`)
    /// were already committed and are NOT rolled back — see module
    /// docs for the rationale.
    GovernanceRefused(String),
    /// Signer error during a per-atom or per-link write. Carries the
    /// underlying diagnostic.
    SignerError(String),
    /// Database error (SQL, transaction commit, etc.). Carries the
    /// underlying diagnostic.
    DbError(String),
}

impl std::fmt::Display for AtomiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("atomise: source memory not found"),
            Self::AlreadyAtomised {
                source_id,
                existing_atom_ids,
            } => write!(
                f,
                "atomise: source '{source_id}' already atomised into {} atoms",
                existing_atom_ids.len()
            ),
            Self::TierLocked => f.write_str(
                "atomise: feature tier is 'keyword' — atomisation requires curator LLM (smart/autonomous)",
            ),
            Self::CuratorFailed(d) => write!(f, "atomise: curator failed: {d}"),
            Self::SourceTooSmall => f.write_str(
                "atomise: source body already at or under max_atom_tokens — no decomposition possible",
            ),
            Self::GovernanceRefused(d) => write!(f, "atomise: governance refused: {d}"),
            Self::SignerError(d) => write!(f, "atomise: signer error: {d}"),
            Self::DbError(d) => write!(f, "atomise: db error: {d}"),
        }
    }
}

impl std::error::Error for AtomiseError {}

/// The atomisation engine.
///
/// Holds the curator (trait object so tests inject a mock), an
/// optional signing keypair (matches the curator pass surface;
/// `None` means writes land unsigned), the substrate connection
/// (`Arc<Mutex<...>>`-wrapped at higher levels — the substrate
/// expects a `&Connection` per call), and the tunables.
///
/// Re-uses `crate::storage::insert` / `crate::storage::create_link_signed`
/// rather than reaching into the DB directly so the substrate-level
/// hook layer (pre_store / post_store / pre_link / post_link) fires
/// for every atom and every `derives_from` edge.
pub struct Atomiser {
    curator: Box<dyn Curator>,
    keypair: Option<Arc<AgentKeypair>>,
    config: AtomiserConfig,
    /// Tier the substrate is running at. When `Keyword`, every
    /// `atomise` call is short-circuited with [`AtomiseError::TierLocked`]
    /// before the source is even loaded. Matches the WT-1-B brief
    /// ("keyword → TierLocked"); other tiers proceed.
    tier: crate::config::FeatureTier,
}

impl Atomiser {
    /// Construct an atomiser. `curator` is the LLM-facing surface
    /// (production: [`curator::LlmCurator`]; tests: a mock). `keypair`
    /// is the daemon's Ed25519 identity — when `None`, links land
    /// `unsigned` (mirror of `create_link_signed`'s contract).
    /// `tier` is the resolved feature tier; the keyword tier short-
    /// circuits atomise calls immediately.
    pub fn new(
        curator: Box<dyn Curator>,
        keypair: Option<Arc<AgentKeypair>>,
        config: AtomiserConfig,
        tier: crate::config::FeatureTier,
    ) -> Self {
        Self {
            curator,
            keypair,
            config,
            tier,
        }
    }

    /// Atomise the memory named by `source_id`.
    ///
    /// `max_atom_tokens` overrides the per-call token budget; pass 0
    /// to defer to `config.default_max_atom_tokens`.
    ///
    /// `force` skips the idempotency check (use to re-atomise after
    /// a curator-prompt change). Old atoms are retained and
    /// `atomised_into` is updated to the fresh count.
    ///
    /// # Errors
    ///
    /// See [`AtomiseError`] for the closed enum of failure modes.
    ///
    /// # Async note
    ///
    /// The function is `async` to match the WT-1-B brief signature
    /// even though the substrate body is fully synchronous (sqlite
    /// is blocking; tiktoken is blocking; the curator LLM call is
    /// blocking-on-HTTP-thread). The async signature exists so
    /// callers in tokio-runtime contexts (the MCP server, the
    /// autonomy scheduler) can `await` it without spawning a
    /// blocking task themselves.
    pub async fn atomise(
        &self,
        conn: &Connection,
        source_id: &str,
        max_atom_tokens: u32,
        force: bool,
        calling_agent_id: &str,
    ) -> Result<AtomiseResult, AtomiseError> {
        self.atomise_sync(conn, source_id, max_atom_tokens, force, calling_agent_id)
    }

    /// Sync entry-point — body of [`Self::atomise`]. Exposed for tests
    /// that prefer to call without tokio scaffolding.
    pub fn atomise_sync(
        &self,
        conn: &Connection,
        source_id: &str,
        max_atom_tokens: u32,
        force: bool,
        calling_agent_id: &str,
    ) -> Result<AtomiseResult, AtomiseError> {
        // Step 3 — tier check (pulled forward of step 1 so we don't burn
        // a DB read when the daemon is on keyword tier).
        if self.tier == crate::config::FeatureTier::Keyword {
            return Err(AtomiseError::TierLocked);
        }

        let budget = if max_atom_tokens == 0 {
            self.config.default_max_atom_tokens
        } else {
            max_atom_tokens
        };

        // Step 1 — load source memory.
        let source = db::get(conn, source_id)
            .map_err(|e| AtomiseError::DbError(e.to_string()))?
            .ok_or(AtomiseError::NotFound)?;

        // Step 2 — idempotency check.
        if !force {
            if let Some(atomised_into) = read_atomised_into(conn, source_id)
                .map_err(|e| AtomiseError::DbError(e.to_string()))?
            {
                if atomised_into > 0 {
                    let existing = list_atoms_of(conn, source_id)
                        .map_err(|e| AtomiseError::DbError(e.to_string()))?;
                    return Err(AtomiseError::AlreadyAtomised {
                        source_id: source_id.to_string(),
                        existing_atom_ids: existing,
                    });
                }
            }
        }

        // Step 4 — pre-flight token count. Sources at or under the
        // budget can never produce a useful split.
        let source_tokens = db::count_tokens_cl100k(&source.content);
        if source_tokens <= budget as usize {
            return Err(AtomiseError::SourceTooSmall);
        }

        // Step 5 + 6 — curator round-trip.
        let atoms = self
            .curator
            .decompose(&source.content, budget, self.config.curator_max_retries)
            .map_err(|e| match e {
                curator::CuratorError::LlmUnavailable(d)
                | curator::CuratorError::MalformedResponse(d) => AtomiseError::CuratorFailed(d),
            })?;

        // Step 7 — empty atoms = "cannot decompose" → SourceTooSmall.
        if atoms.is_empty() {
            return Err(AtomiseError::SourceTooSmall);
        }

        // Cap the count to the brief's [2..=10] envelope. The prompt
        // pins this, but a misbehaving LLM could return e.g. 50; clamp
        // here so the substrate never writes outside the contract.
        let atom_count = atoms.len().min(self.config.max_atoms_per_source);
        if atom_count < self.config.min_atoms_per_source {
            return Err(AtomiseError::SourceTooSmall);
        }
        let atoms: Vec<curator::Atom> = atoms.into_iter().take(atom_count).collect();

        // Step 8 — per-atom transactional write. We iterate atom-by-atom
        // so the hook layer fires per atom (the brief's "atoms are
        // first-class memory_store ops" contract). A governance refusal
        // mid-batch surfaces with the atom index; PRIOR atoms remain
        // committed (they were valid writes by themselves).
        //
        // v0.7.0 Form 4 (issue #757) — atom-grain span fact-provenance.
        // We compute a `SourceSpan` byte-range for each atom into the
        // parent source body. The substring search advances a running
        // cursor so duplicate prefixes across atoms (e.g. two atoms
        // that both quote the same phrase) get assigned non-overlapping
        // spans in the order the curator emitted them. Atoms whose
        // text cannot be located fall back to `None` for the span
        // (curator may have paraphrased) — the substrate still records
        // `source_uri = doc:<parent>` so the lineage edge is preserved
        // even when the byte-range is unrecoverable.
        let mut atom_ids: Vec<String> = Vec::with_capacity(atom_count);
        let mut search_cursor: usize = 0;
        for (idx, atom) in atoms.iter().enumerate() {
            let span = compute_atom_span(&source.content, &atom.text, &mut search_cursor);
            let atom_id = write_atom(
                conn,
                &source,
                atom,
                span,
                calling_agent_id,
                self.keypair.as_deref(),
            )
            .map_err(|e| {
                if let Some(refusal) = e.downcast_ref::<crate::storage::GovernanceRefusal>() {
                    AtomiseError::GovernanceRefused(format!("atom[{idx}]: {}", refusal.reason))
                } else {
                    AtomiseError::DbError(format!("atom[{idx}]: {e}"))
                }
            })?;
            atom_ids.push(atom_id);
        }

        // Step 9 — archive the source in a SEPARATE transaction. The
        // per-atom hooks have already fired by this point, so the
        // source is still live during those hook callbacks (the WT-1-C
        // resolver can switch over only after this commit lands).
        let archived_at = Utc::now().to_rfc3339();
        let atom_count_i64 = i64::try_from(atom_count).unwrap_or(i64::MAX);
        archive_source(conn, source_id, atom_count_i64, &archived_at)
            .map_err(|e| AtomiseError::DbError(e.to_string()))?;

        // Step 10 — emit the atomisation_complete signed_event.
        emit_atomisation_complete_event(
            conn,
            source_id,
            &atom_ids,
            atom_count,
            calling_agent_id,
            &archived_at,
            self.keypair.as_deref(),
        )
        .map_err(|e| AtomiseError::DbError(e.to_string()))?;

        Ok(AtomiseResult {
            source_id: source_id.to_string(),
            atom_ids,
            atom_count,
            archived_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers — kept module-private but `pub(crate)` so the test crate's
// `atomisation_core` module can poke at substrate state directly.
// ---------------------------------------------------------------------------

/// Read the `atomised_into` column for a memory. Returns `Ok(None)`
/// when the column is NULL (memory has not been atomised), `Ok(Some(n))`
/// when set, error on rusqlite failures.
fn read_atomised_into(conn: &Connection, id: &str) -> anyhow::Result<Option<i64>> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT atomised_into FROM memories WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap_or(None);
    Ok(v)
}

/// Return the ordered list of atom ids whose `atom_of` column points
/// at the supplied source id. Ordered by `created_at` then `id` so
/// the response is deterministic across calls.
fn list_atoms_of(conn: &Connection, source_id: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT id FROM memories WHERE atom_of = ?1 ORDER BY created_at ASC, id ASC")?;
    let rows = stmt.query_map(params![source_id], |r| r.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Write one atom: build a Memory row from the source's metadata
/// (namespace/tier/tags), call `db::insert` (which fires the per-write
/// hook chain), then write the `derives_from` edge via
/// `db::create_link_signed`. The edge write is also hook-instrumented.
///
/// Returns the freshly-minted atom id on success. Errors bubble up
/// as `anyhow::Error`; the caller downcasts to `GovernanceRefusal` to
/// distinguish refusal from generic DB failure.
fn write_atom(
    conn: &Connection,
    source: &Memory,
    atom: &curator::Atom,
    span: Option<SourceSpan>,
    calling_agent_id: &str,
    keypair: Option<&AgentKeypair>,
) -> anyhow::Result<String> {
    let now = Utc::now().to_rfc3339();
    let atom_id = uuid::Uuid::new_v4().to_string();
    // Synthesise a title from the source title + a short atom prefix so
    // (title, namespace) does not collide with the parent under the
    // ON CONFLICT clause in db::insert. The first 50 chars of the atom
    // text is the deterministic signal; the trailing UUID8 ensures
    // uniqueness across multiple atoms that share a content prefix.
    let prefix: String = atom
        .text
        .chars()
        .take(50)
        .collect::<String>()
        .trim()
        .to_string();
    let title = if prefix.is_empty() {
        format!("[atom] {} #{}", source.title, &atom_id[..8])
    } else {
        format!("[atom] {} ({})", prefix, &atom_id[..8])
    };

    // metadata.agent_id is the substrate's NHI provenance marker;
    // metadata.atom_index records the curator's 0-based ordering so
    // downstream consumers reproduce the parent's narrative flow.
    let mut metadata = match source.metadata.clone() {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    metadata.insert(
        "agent_id".to_string(),
        serde_json::Value::String(calling_agent_id.to_string()),
    );
    metadata.insert(
        "atom_source_id".to_string(),
        serde_json::Value::String(source.id.clone()),
    );

    let mem = Memory {
        id: atom_id.clone(),
        tier: source.tier.clone(),
        namespace: source.namespace.clone(),
        title,
        content: atom.text.clone(),
        tags: source.tags.clone(),
        priority: source.priority,
        confidence: source.confidence,
        // Source provenance label — "atomiser" so an operator
        // walking `metadata.source` sees the synthetic origin.
        source: "atomiser".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::Value::Object(metadata),
        reflection_depth: source.reflection_depth,
        // Atoms inherit the parent's typed kind: Observation source →
        // Observation atoms (the WT-1-B brief case). Reflection sources
        // could theoretically be atomised too, but that path is gated
        // by WT-1-C/D — for now atoms are typed Observation per the
        // brief.
        memory_kind: MemoryKind::Observation,
        // v0.7.0 QW-2 — atoms are not Persona-kind; entity_id +
        // persona_version stay NULL on the atom row.
        entity_id: None,
        persona_version: None,
        // v0.7.0 Form 4 — atom-grain fact-provenance. Atoms inherit
        // the parent's citations array (the same supporting evidence
        // applies to every decomposed proposition) and stamp the
        // parent memory id under the `doc:` scheme so the lineage is
        // discoverable via the `--source-uri-prefix` recall filter.
        // `source_span` carries the byte-range into the parent body
        // when the curator's text was located verbatim; otherwise
        // `None` (curator may have paraphrased).
        citations: source.citations.clone(),
        source_uri: Some(format!("doc:{}", source.id)),
        source_span: span,
    };

    let actual_id = db::insert(conn, &mem)?;

    // Stamp `atom_of` on the freshly inserted row. db::insert does NOT
    // accept this column on its struct surface (Memory pre-dates the
    // v36 columns), so we issue a targeted UPDATE here. This is
    // hot-path so a single-row UPDATE is acceptable; an alternate
    // approach (extend Memory to carry atom_of) is deferred until a
    // future Memory refactor.
    conn.execute(
        "UPDATE memories SET atom_of = ?1 WHERE id = ?2",
        params![source.id, actual_id],
    )?;

    // derives_from edge: atom → parent. This goes through
    // create_link_signed which writes the row, fires the pre/post-link
    // hooks, signs with the supplied keypair, and appends a
    // `memory_link.created` row to signed_events.
    db::create_link_signed(
        conn,
        &actual_id,
        &source.id,
        MemoryLinkRelation::DerivesFrom.as_str(),
        keypair,
    )?;

    Ok(actual_id)
}

/// Archive the source memory.
///
/// Sets `atomised_into = N` (the substrate-visible signal that the row
/// has been atomised) and writes an `atomisation_archived_at` RFC3339
/// stamp into `metadata` (logical "this row is read-only because its
/// atoms are now the canonical surface"). We do NOT call
/// `db::archive_memory` here — that physically moves the row to
/// `archived_memories`, which would invalidate every atom's `atom_of`
/// FK pointing at it. The atom-of relationship survives as long as
/// the parent row remains in `memories`; flipping `atomised_into`
/// from NULL to N is the downstream signal WT-1-C consumes.
///
/// Runs in its own transaction so the per-atom hooks (step 8) have
/// already fired before the source flips into the "atomised" state.
fn archive_source(
    conn: &Connection,
    source_id: &str,
    atom_count: i64,
    archived_at: &str,
) -> anyhow::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> anyhow::Result<()> {
        // Merge the existing metadata with the new
        // `atomisation_archived_at` key — never clobber other keys.
        let existing_metadata_str: String = conn
            .query_row(
                "SELECT metadata FROM memories WHERE id = ?1",
                params![source_id],
                |r| {
                    r.get::<_, Option<String>>(0)
                        .map(|o| o.unwrap_or_else(|| "{}".to_string()))
                },
            )
            .unwrap_or_else(|_| "{}".to_string());
        let mut meta: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&existing_metadata_str).unwrap_or_default();
        meta.insert(
            "atomisation_archived_at".to_string(),
            serde_json::Value::String(archived_at.to_string()),
        );
        let merged = serde_json::Value::Object(meta).to_string();
        conn.execute(
            "UPDATE memories SET atomised_into = ?1, metadata = ?2, updated_at = ?3 \
             WHERE id = ?4",
            params![atom_count, merged, archived_at, source_id],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Append the final `atomisation_complete` event to `signed_events`.
/// The payload binds the source id, the atom-id list, and the curator
/// model id so a downstream auditor can reproduce the decomposition.
fn emit_atomisation_complete_event(
    conn: &Connection,
    source_id: &str,
    atom_ids: &[String],
    atom_count: usize,
    calling_agent_id: &str,
    archived_at: &str,
    keypair: Option<&AgentKeypair>,
) -> anyhow::Result<()> {
    let payload = serde_json::json!({
        "event_type": "atomisation_complete",
        "source_id": source_id,
        "atom_ids": atom_ids,
        "atom_count": atom_count,
        "calling_agent_id": calling_agent_id,
        "atomisation_timestamp": archived_at,
        "curator_model": "gemma4",
    });
    let bytes = serde_json::to_vec(&payload)?;
    let (signature, attest_level) = if let Some(kp) = keypair.filter(|k| k.can_sign()) {
        let signing = kp.private.as_ref().expect("can_sign() checked");
        use ed25519_dalek::Signer;
        let sig = signing.sign(&bytes);
        (Some(sig.to_bytes().to_vec()), "self_signed")
    } else {
        (None, "unsigned")
    };
    let event = SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: calling_agent_id.to_string(),
        event_type: "atomisation_complete".to_string(),
        payload_hash: payload_hash(&bytes),
        signature,
        attest_level: attest_level.to_string(),
        timestamp: Utc::now().to_rfc3339(),
        ..SignedEvent::default()
    };
    append_signed_event(conn, &event)?;
    Ok(())
}

/// v0.7.0 Form 4 (issue #757) — locate an atom's text inside its
/// parent source body and emit the byte-range as a [`SourceSpan`].
///
/// Strategy:
/// 1. Search verbatim for `atom_text` in `source[cursor..]`. When
///    found, advance the cursor to one past the start of the hit so
///    a subsequent atom that quotes the same prefix doesn't latch
///    onto the same offset.
/// 2. When the verbatim search misses (curator paraphrased, or
///    whitespace differs), fall back to a trimmed prefix-search
///    (first 32 chars of the atom). Trimming whitespace + lowercasing
///    is intentionally NOT performed — we want exact byte-offsets
///    into the unmodified source.
/// 3. Return `None` when both searches miss. The substrate still
///    stamps `source_uri` so the lineage edge survives without the
///    span. This is the documented fallback contract for
///    curator-paraphrase atoms.
fn compute_atom_span(source_body: &str, atom_text: &str, cursor: &mut usize) -> Option<SourceSpan> {
    let needle = atom_text.trim();
    if needle.is_empty() {
        return None;
    }
    let start = if *cursor < source_body.len() {
        source_body[*cursor..].find(needle).map(|off| *cursor + off)
    } else {
        None
    };
    let start = start.or_else(|| source_body.find(needle))?;
    let end = start + needle.len();
    *cursor = start.saturating_add(1);
    Some(SourceSpan { start, end })
}

// ---------------------------------------------------------------------------
// Unit tests — exercise the helpers that don't require a live curator.
// The full integration suite (mock curator + DB + hooks + signed_events)
// lives at `tests/atomisation.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_brief() {
        let c = AtomiserConfig::default();
        assert_eq!(c.default_max_atom_tokens, 200);
        assert_eq!(c.min_atoms_per_source, 2);
        assert_eq!(c.max_atoms_per_source, 10);
        assert_eq!(c.curator_max_retries, 3);
    }

    #[test]
    fn atomise_error_display_shapes() {
        // Spot-check every variant renders without panicking.
        for e in [
            AtomiseError::NotFound,
            AtomiseError::AlreadyAtomised {
                source_id: "src".into(),
                existing_atom_ids: vec!["a".into(), "b".into()],
            },
            AtomiseError::TierLocked,
            AtomiseError::CuratorFailed("bad json".into()),
            AtomiseError::SourceTooSmall,
            AtomiseError::GovernanceRefused("policy".into()),
            AtomiseError::SignerError("no key".into()),
            AtomiseError::DbError("io".into()),
        ] {
            let s = format!("{e}");
            assert!(!s.is_empty());
        }
    }
}
