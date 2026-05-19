// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-2 — Persona-as-artifact engine.
//!
//! A **Persona** is a curator-generated Markdown profile of an entity,
//! synthesised from a cluster of `MemoryKind::Reflection` rows that
//! reference that entity. Personas are the substrate-native expression
//! of Tencent's L3 pattern (PersonaMem 48% → 76%): the substrate
//! distils the agent's reflections about a subject into a stable,
//! recallable artefact so the agent can re-load "what we know about
//! Alice" with a single recall hit instead of paging through dozens of
//! disjoint reflection rows.
//!
//! # Engine surface
//!
//! ```ignore
//! use ai_memory::persona::{PersonaConfig, PersonaGenerator};
//!
//! let cfg = PersonaConfig::default();
//! let mut gen = PersonaGenerator::new(&conn, &llm, signer, cfg);
//! let persona = generator.generate("entity-alice", "team/alpha")?;
//! ```
//!
//! # Persona body shape
//!
//! The curator returns a 300–500 word Markdown body. Every claim is
//! footnoted via `[^ref]` citations whose anchor points at the source
//! reflection's UUID — operators inspecting `~/.ai-memory/personas/...`
//! can follow the link back to the originating reflection via
//! `ai-memory get <id>`. The Persona row carries `entity_id`,
//! `persona_version`, and a `metadata.persona` envelope that pins:
//!
//!   * `entity_id` (redundant with the SQL column for legacy readers),
//!   * `sources: [reflection_id, …]`,
//!   * `version` (also pinned on the SQL column),
//!   * `attest_level` summarising the strongest attestation across
//!     `derives_from` edges (mirrors QW-1's reflection-export shape).
//!
//! # Provenance
//!
//! Each generation emits one `derives_from` `memory_link` per source
//! reflection so the KG walker (`memory_find_paths`, `memory_kg_query`)
//! can follow the Persona → Reflection → Observation chain end-to-end.
//! A `persona_generated` row is appended to `signed_events` with the
//! sources hash; the H5 audit chain captures every regeneration as a
//! distinct, signed event.

use crate::models::ConfidenceSource;
use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::autonomy::AutonomyLlm;
use crate::identity::keypair::AgentKeypair;
use crate::identity::sign::{SignablePersona, sign_persona};
use crate::models::{Memory, MemoryKind, Tier};
use crate::signed_events::{SignedEvent, append_signed_event};
use crate::storage as db;
use crate::validate;

/// Default ceiling on how many reflections feed a single persona
/// generation. Mirrors the prompt budget — Gemma 4 produces tighter
/// summaries when the source pool stays in single digits to low-20s.
pub const DEFAULT_MAX_REFLECTION_SOURCES: usize = 20;

/// Default curator family stamp on the Persona's `metadata.agent_id`
/// when the engine is constructed without an explicit keypair (tests).
const ANONYMOUS_CURATOR_AGENT_ID: &str = "ai:curator";

/// v0.7.0 issue #848 — sentinel namespace value reported in
/// [`PersonaError::NoReflections`] when the caller asked for the
/// cross-namespace aggregation path and zero matching reflections
/// existed in ANY namespace. Distinct from any real namespace string
/// (`"global"`, `"team/alpha"`, etc.) so an operator-facing error
/// message can distinguish "no reflections in namespace 'X'" from
/// "no reflections anywhere in the substrate".
pub const CROSS_NAMESPACE_SENTINEL: &str = "<any namespace>";

/// v0.7.0 issue #848 — namespace-scope discriminator for
/// [`PersonaGenerator::generate_in_scope`]. Single-namespace mode
/// preserves the pre-#848 behaviour; `AnyTargeting(ns)` aggregates
/// every reflection that mentions the entity across all namespaces
/// and lands the new persona row in `ns`.
#[derive(Debug, Clone, Copy)]
enum PersonaScope<'a> {
    Single(&'a str),
    AnyTargeting(&'a str),
}

/// Static configuration for [`PersonaGenerator`].
#[derive(Debug, Clone)]
pub struct PersonaConfig {
    /// Maximum number of source reflections the curator considers per
    /// generation. Defaults to [`DEFAULT_MAX_REFLECTION_SOURCES`].
    pub max_reflection_sources: usize,
    /// Persona memories land at this tier. Defaults to `Tier::Long` —
    /// personas are the curator's high-confidence distillation and the
    /// substrate keeps them around indefinitely.
    pub tier: Tier,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            max_reflection_sources: DEFAULT_MAX_REFLECTION_SOURCES,
            tier: Tier::Long,
        }
    }
}

/// Public persona shape returned by [`PersonaGenerator::generate`] and
/// surfaced over the MCP `memory_persona` read-only tool.
///
/// Mirrors the SQL row's columns plus the rendered Markdown body and
/// the source-id list spliced into `metadata.persona`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Persona {
    /// The Persona memory's id (UUIDv4). Stable per (entity_id,
    /// namespace, version) tuple.
    pub id: String,
    /// Subject of the persona.
    pub entity_id: String,
    /// Namespace the persona was minted under.
    pub namespace: String,
    /// 300–500 word Markdown body with `[^ref]` footnotes.
    pub body_md: String,
    /// Source reflection ids — one `derives_from` edge per element.
    pub sources: Vec<String>,
    /// RFC3339 generation timestamp.
    pub generated_at: String,
    /// Monotonic version counter — `1` on the first generation, then
    /// `prev + 1` per regeneration.
    pub version: i32,
    /// Strongest attestation level across the `derives_from` edges.
    /// Mirrors QW-1's `attest_level` summary on reflection exports.
    pub attest_level: String,
}

/// Errors returned by [`PersonaGenerator::generate`].
#[derive(Debug)]
pub enum PersonaError {
    /// Input validation failure (empty entity_id, malformed namespace).
    Validation(String),
    /// The entity has no reflections in this namespace.
    NoReflections {
        entity_id: String,
        namespace: String,
    },
    /// The curator LLM failed during synthesis.
    Llm(String),
    /// A SQL operation failed.
    Db(anyhow::Error),
}

impl fmt::Display for PersonaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(msg) => write!(f, "persona validation failed: {msg}"),
            Self::NoReflections {
                entity_id,
                namespace,
            } => write!(
                f,
                "no reflections found for entity '{entity_id}' in namespace '{namespace}'"
            ),
            Self::Llm(msg) => write!(f, "curator synthesis failed: {msg}"),
            Self::Db(e) => write!(f, "persona db error: {e}"),
        }
    }
}

impl std::error::Error for PersonaError {}

impl From<anyhow::Error> for PersonaError {
    fn from(e: anyhow::Error) -> Self {
        Self::Db(e)
    }
}

impl From<rusqlite::Error> for PersonaError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(anyhow::Error::from(e))
    }
}

/// The persona-generation engine.
///
/// Constructed per call (cheap — just holds references). Generation is
/// idempotent in the sense that calling `generate` twice writes two
/// distinct rows with consecutive `version` numbers; the substrate
/// never overwrites a persona in place so audit trails stay intact.
pub struct PersonaGenerator<'a> {
    conn: &'a Connection,
    llm: &'a dyn AutonomyLlm,
    signer: Option<&'a AgentKeypair>,
    config: PersonaConfig,
}

impl<'a> PersonaGenerator<'a> {
    /// Construct a fresh generator.
    pub fn new(
        conn: &'a Connection,
        llm: &'a dyn AutonomyLlm,
        signer: Option<&'a AgentKeypair>,
        config: PersonaConfig,
    ) -> Self {
        Self {
            conn,
            llm,
            signer,
            config,
        }
    }

    /// Resolve the curator agent_id stamped on every persona this
    /// generator writes. Falls back to [`ANONYMOUS_CURATOR_AGENT_ID`]
    /// when no keypair is configured (test paths).
    fn agent_id(&self) -> String {
        self.signer
            .map(|kp| kp.agent_id.clone())
            .unwrap_or_else(|| ANONYMOUS_CURATOR_AGENT_ID.to_string())
    }

    /// Generate a fresh Persona for `entity_id` in `namespace`.
    ///
    /// # Steps
    ///
    /// 1. Validate `entity_id` (non-empty, within identity bounds) and
    ///    `namespace`.
    /// 2. Load up to `config.max_reflection_sources` Reflection-kind
    ///    memories from `namespace` referencing the entity.
    /// 3. Refuse with [`PersonaError::NoReflections`] when the pool is
    ///    empty — a Persona without sources has no audit trail.
    /// 4. Resolve the next `version` (max existing + 1, defaulting 1).
    /// 5. Call the curator (`AutonomyLlm::summarize_memories`) over
    ///    the sources to produce the Markdown body.
    /// 6. Insert a `MemoryKind::Persona` memory row with `entity_id` +
    ///    `persona_version` populated and metadata carrying the
    ///    `persona` envelope.
    /// 7. Write one `derives_from` `memory_link` from the persona
    ///    row to each source reflection.
    /// 8. Append a `persona_generated` row to `signed_events`.
    ///
    /// # Errors
    ///
    /// One of the [`PersonaError`] variants. The DB-level errors are
    /// the only ones without a structured payload — every other
    /// variant carries enough context for a clean operator message.
    pub fn generate(
        &self,
        entity_id: &str,
        namespace: &str,
    ) -> std::result::Result<Persona, PersonaError> {
        self.generate_in_scope(entity_id, PersonaScope::Single(namespace))
    }

    /// v0.7.0 issue #848 — cross-namespace persona generation.
    ///
    /// Equivalent to [`Self::generate`] except the source-reflection
    /// scan is broadened to every namespace the substrate stores. The
    /// new persona row lands in `target_namespace` (callers
    /// typically pass `"global"` so subsequent
    /// `memory_persona(entity_id)` reads have a deterministic
    /// landing zone). Use this when an NHI agent has spread its
    /// reflections across multiple namespaces (e.g.
    /// `global/policies`, `ai-memory/v0.7.0-nhi-testing`, project
    /// buckets) and needs a single Persona that aggregates the full
    /// identity arc.
    ///
    /// # Errors
    ///
    /// Same envelope as [`Self::generate`]. When zero matching
    /// reflections exist in ANY namespace, the
    /// `NoReflections.namespace` field is the
    /// [`CROSS_NAMESPACE_SENTINEL`] string.
    pub fn generate_cross_namespace(
        &self,
        entity_id: &str,
        target_namespace: &str,
    ) -> std::result::Result<Persona, PersonaError> {
        self.generate_in_scope(entity_id, PersonaScope::AnyTargeting(target_namespace))
    }

    /// Internal common path. Routes to the appropriate source loader
    /// based on `scope`; centralises validation, version bump,
    /// curator invocation, write, link emission, and signed-events
    /// stamping so the single-namespace and cross-namespace entry
    /// points stay in lockstep for free.
    fn generate_in_scope(
        &self,
        entity_id: &str,
        scope: PersonaScope<'_>,
    ) -> std::result::Result<Persona, PersonaError> {
        validate_entity_id(entity_id)?;
        let namespace = match scope {
            PersonaScope::Single(ns) | PersonaScope::AnyTargeting(ns) => ns,
        };
        validate::validate_namespace(namespace)
            .map_err(|e| PersonaError::Validation(e.to_string()))?;

        let sources = match scope {
            PersonaScope::Single(ns) => load_reflections_for_entity(
                self.conn,
                entity_id,
                ns,
                self.config.max_reflection_sources,
            )?,
            PersonaScope::AnyTargeting(_) => load_reflections_for_entity_any_namespace(
                self.conn,
                entity_id,
                self.config.max_reflection_sources,
            )?,
        };
        if sources.is_empty() {
            let reported_ns = match scope {
                PersonaScope::Single(ns) => ns.to_string(),
                PersonaScope::AnyTargeting(_) => CROSS_NAMESPACE_SENTINEL.to_string(),
            };
            return Err(PersonaError::NoReflections {
                entity_id: entity_id.to_string(),
                namespace: reported_ns,
            });
        }

        let version = next_version(self.conn, entity_id, namespace)?;

        // Curator synthesis — `AutonomyLlm::summarize_memories` is the
        // narrow LLM trait every other curator pass already uses; mock
        // implementations in `llm::test_support` keep tests
        // deterministic without spinning up Ollama.
        let llm_input: Vec<(String, String)> = sources
            .iter()
            .map(|m| (m.title.clone(), m.content.clone()))
            .collect();
        let body_md_raw = self
            .llm
            .summarize_memories(&llm_input)
            .map_err(|e| PersonaError::Llm(e.to_string()))?;
        let body_md = render_body_with_footnotes(&body_md_raw, &sources);

        let now = Utc::now().to_rfc3339();
        let agent_id = self.agent_id();
        let title = persona_title(entity_id, version);
        let source_ids: Vec<String> = sources.iter().map(|m| m.id.clone()).collect();

        // v0.7.0 issue #810 / #811 / #812 — the in-flight `attest_level`
        // is unknown until after the `derived_from` link writes complete
        // (BUG-A's atomic invariant means the row tells the truth about
        // its signature). We stamp a provisional metadata envelope now,
        // then patch `attest_level` + `signature` in place once the
        // post-link computation finishes. This keeps the write order
        // (memory → links → metadata patch → signed_events) auditable.
        let persona_id_local = uuid::Uuid::new_v4().to_string();

        let mut metadata = serde_json::json!({
            "agent_id": agent_id,
            "persona": {
                "entity_id": entity_id,
                "sources": source_ids.clone(),
                "version": version,
                "attest_level": "unsigned",
                "generated_at": now,
            }
        });

        let persona_mem = Memory {
            id: persona_id_local.clone(),
            tier: self.config.tier.clone(),
            namespace: namespace.to_string(),
            title,
            content: body_md.clone(),
            tags: vec!["persona".to_string()],
            priority: 7,
            confidence: 1.0,
            source: "curator".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_accessed_at: None,
            expires_at: None,
            metadata: metadata.clone(),
            reflection_depth: 0,
            memory_kind: MemoryKind::Persona,
            entity_id: Some(entity_id.to_string()),
            persona_version: Some(version),
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };

        let persona_id = db::insert(self.conn, &persona_mem)
            .with_context(|| format!("inserting persona for {entity_id} v{version}"))?;

        // v0.7.0 issue #811 — `derived_from` edges must use the
        // signing-aware `create_link_signed` shim. The pre-#811 code
        // path passed `None` here unconditionally even when the
        // generator was constructed with a keypair; that regression
        // is what produced unsigned link rows alongside a curator
        // whose daemon already owned a private key.
        for source in &sources {
            db::create_link_signed(
                self.conn,
                &persona_id,
                &source.id,
                "derived_from",
                self.signer,
            )
            .with_context(|| format!("linking persona {persona_id} -> source {}", source.id))?;
        }

        // v0.7.0 issue #812 — sign the persona artifact end-to-end when
        // the generator was constructed with a signing keypair. The
        // signature commits to the seven-field SignablePersona envelope
        // (persona_id, entity_id, namespace, version, generated_at,
        // sources, body_md_sha256). The body hash is computed over the
        // rendered Markdown so the bounded payload size is independent
        // of body length.
        let body_hash = {
            let mut h = Sha256::new();
            h.update(body_md.as_bytes());
            let mut out = [0u8; 32];
            out.copy_from_slice(&h.finalize());
            out
        };

        let signature_bytes: Option<Vec<u8>> = match self.signer {
            Some(kp) if kp.can_sign() => {
                let p = SignablePersona {
                    persona_id: persona_id.as_str(),
                    entity_id,
                    namespace,
                    version,
                    generated_at: now.as_str(),
                    sources: &source_ids,
                    body_md_sha256: &body_hash,
                };
                Some(sign_persona(kp, &p).context("sign persona artifact")?)
            }
            _ => None,
        };

        // Resolve the effective `attest_level` for the persona from the
        // `derived_from` link rows we just wrote. The strongest level
        // across the source edges is the level the persona truthfully
        // bears — a Persona whose links are unsigned cannot honestly
        // claim `self_signed` even when the curator stamps a signature
        // on its own body. The match between the curator's signing
        // status and the source edges' signing status keeps the
        // wire-level `attest_level` strictly monotone.
        let link_attest = db::strongest_attest_level_for_source(self.conn, &persona_id)
            .context("resolve strongest link attest_level")?;
        let attest_level = if signature_bytes.is_some() {
            // The persona body itself is signed. Prefer the stronger of
            // the two labels (`peer_attested` beats `self_signed`).
            match link_attest.as_str() {
                "peer_attested" => "peer_attested".to_string(),
                _ => "self_signed".to_string(),
            }
        } else {
            link_attest
        };

        // Patch the metadata envelope in place — the wire response
        // returned to the caller, the `metadata.persona.attest_level`
        // field used by `get_latest_persona` readers, and the
        // base64-encoded signature all flow from here.
        if let Some(env) = metadata
            .get_mut("persona")
            .and_then(serde_json::Value::as_object_mut)
        {
            env.insert(
                "attest_level".to_string(),
                serde_json::Value::String(attest_level.clone()),
            );
            if let Some(sig) = signature_bytes.as_ref() {
                env.insert(
                    "signature".to_string(),
                    serde_json::Value::String(BASE64_STANDARD.encode(sig)),
                );
            }
        }
        let new_metadata_str = serde_json::to_string(&metadata)
            .context("serialise updated persona metadata envelope")?;
        self.conn
            .execute(
                "UPDATE memories SET metadata = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![new_metadata_str, &now, &persona_id],
            )
            .context("patch persona metadata with signature/attest_level")?;

        emit_persona_generated_event(
            self.conn,
            &persona_id,
            &agent_id,
            &source_ids,
            &now,
            signature_bytes.as_deref(),
            &attest_level,
        )?;

        Ok(Persona {
            id: persona_id,
            entity_id: entity_id.to_string(),
            namespace: namespace.to_string(),
            body_md,
            sources: source_ids,
            generated_at: now,
            version,
            attest_level,
        })
    }
}

/// Validate that `entity_id` is non-empty and inside the same length
/// envelope `validate::validate_agent_id` enforces — operators
/// frequently reuse the same identifier for both, so the validation
/// rule stays symmetric.
fn validate_entity_id(entity_id: &str) -> std::result::Result<(), PersonaError> {
    if entity_id.trim().is_empty() {
        return Err(PersonaError::Validation("entity_id cannot be empty".into()));
    }
    if entity_id.len() > 128 {
        return Err(PersonaError::Validation(format!(
            "entity_id exceeds 128 characters (got {})",
            entity_id.len()
        )));
    }
    Ok(())
}

/// Read the most recent persona for `(entity_id, namespace)`, returning
/// `None` when the entity has never had a persona minted.
///
/// Used by the `memory_persona` read-only MCP tool and by the
/// `ai-memory persona <entity_id>` CLI command. Indexed lookup via
/// `idx_personas_by_entity`.
pub fn get_latest_persona(
    conn: &Connection,
    entity_id: &str,
    namespace: &str,
) -> Result<Option<Persona>> {
    let mut stmt = conn.prepare(
        "SELECT id, entity_id, namespace, content, created_at, COALESCE(persona_version, 1), metadata
         FROM memories
         WHERE memory_kind = 'persona'
           AND entity_id = ?1
           AND namespace = ?2
         ORDER BY COALESCE(persona_version, 0) DESC, created_at DESC
         LIMIT 1",
    )?;
    let row: Option<(String, String, String, String, String, i32, String)> = stmt
        .query_row(rusqlite::params![entity_id, namespace], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .ok();
    let Some((id, entity_id, namespace, body_md, generated_at, version, metadata_str)) = row else {
        return Ok(None);
    };
    let meta: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or_else(|_| serde_json::json!({}));
    let envelope = meta.get("persona").cloned().unwrap_or_default();
    let sources = envelope
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let attest_level = envelope
        .get("attest_level")
        .and_then(|v| v.as_str())
        .unwrap_or("unsigned")
        .to_string();
    Ok(Some(Persona {
        id,
        entity_id,
        namespace,
        body_md,
        sources,
        generated_at,
        version,
        attest_level,
    }))
}

/// Resolve the next `persona_version` for `(entity_id, namespace)`.
/// Returns `1` when no prior persona exists for the pair.
fn next_version(conn: &Connection, entity_id: &str, namespace: &str) -> Result<i32> {
    let v: Option<i32> = conn
        .query_row(
            "SELECT COALESCE(MAX(persona_version), 0)
             FROM memories
             WHERE memory_kind = 'persona'
               AND entity_id = ?1
               AND namespace = ?2",
            rusqlite::params![entity_id, namespace],
            |r| r.get(0),
        )
        .optional_default(0_i32);
    Ok(v.map(|n| n + 1).unwrap_or(1))
}

/// Load up to `limit` reflection-kind memories from `namespace` whose
/// stored `mentioned_entity_id` matches.
///
/// v0.7.0 polish PERF-8 (issue #781): previously this matched candidate
/// reflections with `(title|content|metadata) LIKE '%<entity>%'` — a
/// full-table scan across three TEXT columns for every reflection in
/// the namespace. The fix relies on the schema-v42
/// `mentioned_entity_id` column (populated at write time by
/// [`crate::storage::extract_mentioned_entity_id`]; backfilled for
/// legacy rows by the v42 migration) so the source pool is loaded via
/// an indexed equality lookup against
/// `idx_memories_mentioned_entity`. The query plan changes from
/// `SCAN memories` to `SEARCH memories USING INDEX
/// idx_memories_mentioned_entity (mentioned_entity_id=? AND namespace=?)`.
///
/// The lookup is bounded by `limit` so a runaway namespace can't blow
/// the prompt budget.
fn load_reflections_for_entity(
    conn: &Connection,
    entity_id: &str,
    namespace: &str,
    limit: usize,
) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, namespace, title, content, tags, priority, confidence, source,
                access_count, created_at, updated_at, last_accessed_at, expires_at,
                metadata, COALESCE(reflection_depth, 0), COALESCE(memory_kind, 'observation'),
                entity_id, persona_version
         FROM memories
         WHERE namespace = ?1
           AND memory_kind = 'reflection'
           AND mentioned_entity_id = ?2
         ORDER BY priority DESC, created_at DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            namespace,
            entity_id,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ],
        crate::storage::row_to_memory,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// v0.7.0 issue #848 — cross-namespace reflection loader. Identical
/// to [`load_reflections_for_entity`] minus the `namespace = ?`
/// predicate so a single query aggregates every reflection that
/// references the entity regardless of which namespace it lives in.
/// Still rides the `idx_memories_mentioned_entity` PERF-8 index
/// (mentioned_entity_id is the leading column). Bounded by `limit`.
fn load_reflections_for_entity_any_namespace(
    conn: &Connection,
    entity_id: &str,
    limit: usize,
) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, tier, namespace, title, content, tags, priority, confidence, source,
                access_count, created_at, updated_at, last_accessed_at, expires_at,
                metadata, COALESCE(reflection_depth, 0), COALESCE(memory_kind, 'observation'),
                entity_id, persona_version
         FROM memories
         WHERE memory_kind = 'reflection'
           AND mentioned_entity_id = ?1
         ORDER BY priority DESC, created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![entity_id, i64::try_from(limit).unwrap_or(i64::MAX)],
        crate::storage::row_to_memory,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Compose the on-disk Markdown body. Appends a footer with one
/// `[^N]: <reflection-id>` line per source so every citation in the
/// raw body renders as a clickable footnote in standard Markdown
/// viewers.
fn render_body_with_footnotes(raw: &str, sources: &[Memory]) -> String {
    let mut out = String::with_capacity(raw.len() + sources.len() * 64);
    out.push_str(raw.trim_end());
    out.push_str("\n\n## Sources\n\n");
    for (idx, src) in sources.iter().enumerate() {
        // 1-based citation index keeps Markdown readers happy.
        out.push_str(&format!("[^{}]: {} — `{}`\n", idx + 1, src.title, src.id));
    }
    out
}

/// Title format used for Persona memories. Embeds the version so the
/// (title, namespace) uniqueness constraint never trips between
/// generations.
fn persona_title(entity_id: &str, version: i32) -> String {
    format!("__persona_{entity_id}_v{version}")
}

/// Append a `persona_generated` row to the H5 audit chain so an
/// auditor walking `signed_events` can replay every persona mint /
/// regeneration with provenance over the source-id list.
///
/// v0.7.0 issue #812 / #813 — when the persona was signed, `signature`
/// carries the 64-byte Ed25519 bytes and `attest_level` stamps the
/// label the persona ended up with (`self_signed` or `peer_attested`).
/// When the persona was unsigned both fall back to `None` / `unsigned`
/// preserving the pre-#812 ledger shape.
fn emit_persona_generated_event(
    conn: &Connection,
    persona_id: &str,
    agent_id: &str,
    sources: &[String],
    now: &str,
    signature: Option<&[u8]>,
    attest_level: &str,
) -> Result<()> {
    let mut hasher = Sha256::new();
    hasher.update(persona_id.as_bytes());
    hasher.update(b"\x1f");
    for src in sources {
        hasher.update(src.as_bytes());
        hasher.update(b"\x1f");
    }
    let payload_hash = hasher.finalize().to_vec();
    let event = SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        event_type: "persona_generated".to_string(),
        payload_hash,
        signature: signature.map(<[u8]>::to_vec),
        attest_level: attest_level.to_string(),
        timestamp: now.to_string(),
        ..SignedEvent::default()
    };
    append_signed_event(conn, &event)
}

/// Render a YAML-frontmatter Markdown export of a persona — mirrors
/// QW-1's reflection-export envelope so operators can `cat` a
/// persona alongside reflections from the same directory tree.
#[must_use]
pub fn render_persona_md(persona: &Persona) -> String {
    let mut out = String::with_capacity(persona.body_md.len() + 256);
    out.push_str("---\n");
    out.push_str(&format!("memory_id: {}\n", persona.id));
    out.push_str(&format!("entity_id: {}\n", persona.entity_id));
    out.push_str(&format!("namespace: {}\n", persona.namespace));
    out.push_str(&format!("persona_version: {}\n", persona.version));
    out.push_str(&format!("generated_at: {}\n", persona.generated_at));
    out.push_str(&format!("attest_level: {}\n", persona.attest_level));
    out.push_str(&format!("sources: {}\n", persona.sources.len()));
    out.push_str("---\n\n");
    out.push_str(&persona.body_md);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Render a structured JSON envelope mirroring [`render_persona_md`].
/// Field order is stable for the test snapshot — we build a
/// `BTreeMap`-backed `Value` so callers can pin the wire shape.
#[must_use]
pub fn render_persona_json(persona: &Persona) -> String {
    let mut map: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    map.insert("memory_id", serde_json::Value::String(persona.id.clone()));
    map.insert(
        "entity_id",
        serde_json::Value::String(persona.entity_id.clone()),
    );
    map.insert(
        "namespace",
        serde_json::Value::String(persona.namespace.clone()),
    );
    map.insert(
        "persona_version",
        serde_json::Value::Number(serde_json::Number::from(persona.version)),
    );
    map.insert(
        "generated_at",
        serde_json::Value::String(persona.generated_at.clone()),
    );
    map.insert(
        "attest_level",
        serde_json::Value::String(persona.attest_level.clone()),
    );
    map.insert(
        "sources",
        serde_json::Value::Array(
            persona
                .sources
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
    );
    map.insert(
        "body_md",
        serde_json::Value::String(persona.body_md.clone()),
    );
    serde_json::to_string_pretty(&map).unwrap_or_else(|_| "{}".to_string())
}

// ---------------------------------------------------------------------------
// Local helper: optional_default style ergonomic shim
// ---------------------------------------------------------------------------

trait OptionalDefault<T> {
    fn optional_default(self, default: T) -> Option<T>;
}

impl<T> OptionalDefault<T> for std::result::Result<T, rusqlite::Error>
where
    T: Default,
{
    fn optional_default(self, default: T) -> Option<T> {
        match self {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Some(default),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::test_support::MockOllamaClient;
    use crate::models::{Memory, MemoryKind, Tier};
    use crate::storage as db;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn fresh_db() -> (Connection, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir)
    }

    /// Mock implementation of `AutonomyLlm` that returns a canned
    /// summary keyed off the source titles — deterministic, no Ollama
    /// dependency.
    struct StubLlm {
        canned: String,
    }

    impl AutonomyLlm for StubLlm {
        fn auto_tag(&self, _title: &str, _content: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn detect_contradiction(&self, _a: &str, _b: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn summarize_memories(&self, memories: &[(String, String)]) -> anyhow::Result<String> {
            // Echo back the source count so tests can assert the
            // generator passed the right shape to the curator.
            Ok(format!("{} [from {} sources]", self.canned, memories.len()))
        }
    }

    fn seed_reflection(conn: &Connection, namespace: &str, title: &str, body: &str) -> String {
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: title.to_string(),
            content: body.to_string(),
            tags: vec!["reflection".into()],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "ai:test"}),
            reflection_depth: 1,
            memory_kind: MemoryKind::Reflection,
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
        db::insert(conn, &mem).unwrap()
    }

    #[test]
    fn validate_entity_id_rejects_empty() {
        assert!(matches!(
            validate_entity_id(""),
            Err(PersonaError::Validation(_))
        ));
        assert!(matches!(
            validate_entity_id("   "),
            Err(PersonaError::Validation(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_overlong() {
        let long = "x".repeat(129);
        assert!(matches!(
            validate_entity_id(&long),
            Err(PersonaError::Validation(_))
        ));
    }

    #[test]
    fn validate_entity_id_accepts_normal_ids() {
        assert!(validate_entity_id("alice").is_ok());
        assert!(validate_entity_id("entity-42").is_ok());
    }

    #[test]
    fn generate_refuses_when_no_reflections() {
        let (conn, _dir) = fresh_db();
        let llm = StubLlm {
            canned: "irrelevant".into(),
        };
        let generator = PersonaGenerator::new(&conn, &llm, None, PersonaConfig::default());
        let err = generator.generate("alice", "team/alpha").unwrap_err();
        assert!(matches!(err, PersonaError::NoReflections { .. }));
    }

    #[test]
    fn render_body_with_footnotes_appends_sources_block() {
        let (conn, _dir) = fresh_db();
        let id1 = seed_reflection(&conn, "team/alpha", "ref-1 about alice", "alice does X");
        let id2 = seed_reflection(&conn, "team/alpha", "ref-2 about alice", "alice does Y");
        let mems = vec![
            db::get(&conn, &id1).unwrap().unwrap(),
            db::get(&conn, &id2).unwrap().unwrap(),
        ];
        let body = render_body_with_footnotes("Alice is composed and thoughtful.", &mems);
        assert!(body.contains("## Sources"));
        assert!(body.contains(&format!("[^1]: ref-1 about alice — `{id1}`")));
        assert!(body.contains(&format!("[^2]: ref-2 about alice — `{id2}`")));
    }

    #[test]
    fn next_version_starts_at_one_then_increments() {
        let (conn, _dir) = fresh_db();
        assert_eq!(next_version(&conn, "alice", "team/alpha").unwrap(), 1);
        // Seed a persona row directly to bump version state.
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "team/alpha".into(),
            title: persona_title("alice", 1),
            content: "x".into(),
            tags: vec![],
            priority: 7,
            confidence: 1.0,
            source: "curator".into(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: MemoryKind::Persona,
            entity_id: Some("alice".into()),
            persona_version: Some(1),
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };
        db::insert(&conn, &mem).unwrap();
        assert_eq!(next_version(&conn, "alice", "team/alpha").unwrap(), 2);
    }

    #[test]
    fn render_persona_md_includes_frontmatter() {
        let p = Persona {
            id: "p1".into(),
            entity_id: "alice".into(),
            namespace: "team/alpha".into(),
            body_md: "Alice is composed.".into(),
            sources: vec!["s1".into(), "s2".into()],
            generated_at: "2026-05-15T00:00:00Z".into(),
            version: 1,
            attest_level: "unsigned".into(),
        };
        let md = render_persona_md(&p);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("memory_id: p1\n"));
        assert!(md.contains("entity_id: alice\n"));
        assert!(md.contains("namespace: team/alpha\n"));
        assert!(md.contains("persona_version: 1\n"));
        assert!(md.contains("sources: 2\n"));
        assert!(md.contains("Alice is composed."));
    }

    #[test]
    fn render_persona_json_round_trips() {
        let p = Persona {
            id: "p1".into(),
            entity_id: "alice".into(),
            namespace: "team/alpha".into(),
            body_md: "body".into(),
            sources: vec!["s1".into()],
            generated_at: "2026-05-15T00:00:00Z".into(),
            version: 2,
            attest_level: "unsigned".into(),
        };
        let s = render_persona_json(&p);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["memory_id"], "p1");
        assert_eq!(v["entity_id"], "alice");
        assert_eq!(v["persona_version"], 2);
    }

    #[test]
    fn mock_llm_available() {
        // Smoke test that the project's mock LLM scaffolding is reachable.
        let _ = MockOllamaClient::new_with_url("http://localhost:11434", "gemma2:2b").unwrap();
    }

    // -------------------------------------------------------------------------
    // v0.7.0 issue #810 / #811 / #812 / #813 — signing-path unit coverage
    // -------------------------------------------------------------------------

    /// Mint two reflections tagged with `entity_id = alice` so the
    /// PERF-8 `mentioned_entity_id` lookup matches. Returns the seeded
    /// source ids for downstream assertion.
    fn seed_two_alice_reflections(conn: &Connection, namespace: &str) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..2 {
            let now = Utc::now().to_rfc3339();
            let mem = Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: Tier::Mid,
                namespace: namespace.to_string(),
                title: format!("obs-{i} about alice"),
                content: format!("alice did thing {i}"),
                tags: vec!["reflection".into()],
                priority: 5,
                confidence: 1.0,
                source: "test".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({"agent_id": "ai:test", "entity_id": "alice"}),
                reflection_depth: 1,
                memory_kind: MemoryKind::Reflection,
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
            ids.push(db::insert(conn, &mem).unwrap());
        }
        ids
    }

    #[test]
    fn generate_unsigned_path_writes_unsigned_links() {
        // Pre-#811: every code path through PersonaGenerator wrote
        // links via `db::create_link` (the unsigned shim). The
        // intentional "passes None as signer" behaviour stays correct
        // — the link column AND the persona metadata agree on
        // "unsigned" instead of disagreeing.
        let (conn, _dir) = fresh_db();
        seed_two_alice_reflections(&conn, "team/alpha");
        let llm = StubLlm {
            canned: "Alice is methodical.".into(),
        };
        let generator = PersonaGenerator::new(&conn, &llm, None, PersonaConfig::default());
        let persona = generator.generate("alice", "team/alpha").expect("generate");
        assert_eq!(persona.attest_level, "unsigned");
        // Every `derived_from` link rooted at the persona must say
        // `unsigned` (matching the absent signer).
        let links: Vec<(Option<Vec<u8>>, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT signature, attest_level FROM memory_links \
                     WHERE source_id = ?1 AND relation = 'derived_from'",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![&persona.id], |r| {
                Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        assert_eq!(links.len(), 2);
        for (sig, level) in &links {
            assert!(sig.is_none(), "unsigned link must have NULL signature");
            assert_eq!(level, "unsigned");
        }
    }

    #[test]
    fn generate_signed_path_writes_signed_links_and_metadata() {
        // BUG-B + BUG-C closing test — when a keypair is wired into
        // PersonaGenerator the `derived_from` links land signed, the
        // persona's metadata carries the base64 signature, and the
        // returned struct stamps `self_signed` on `attest_level`.
        let (conn, _dir) = fresh_db();
        seed_two_alice_reflections(&conn, "team/alpha");
        let kp = crate::identity::keypair::generate("ai:curator").unwrap();
        let llm = StubLlm {
            canned: "Signed alice body".into(),
        };
        let generator = PersonaGenerator::new(&conn, &llm, Some(&kp), PersonaConfig::default());
        let persona = generator.generate("alice", "team/alpha").expect("generate");
        assert_eq!(persona.attest_level, "self_signed");

        // Links: all signed, 64-byte signature on each row.
        let links: Vec<(Option<Vec<u8>>, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT signature, attest_level FROM memory_links \
                     WHERE source_id = ?1 AND relation = 'derived_from'",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![&persona.id], |r| {
                Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
        };
        assert_eq!(links.len(), 2);
        for (sig, level) in &links {
            assert_eq!(level, "self_signed");
            let sig_bytes = sig.as_ref().expect("signed link must have signature blob");
            assert_eq!(sig_bytes.len(), 64, "Ed25519 signatures are 64 bytes");
        }

        // Persona metadata carries base64 signature + matching
        // attest_level + matching agent_id (curator).
        let meta_str: String = conn
            .query_row(
                "SELECT metadata FROM memories WHERE id = ?1",
                rusqlite::params![&persona.id],
                |r| r.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
        assert_eq!(meta["agent_id"], "ai:curator");
        assert_eq!(meta["persona"]["attest_level"], "self_signed");
        let b64 = meta["persona"]["signature"]
            .as_str()
            .expect("metadata.persona.signature must be a string");
        let decoded = BASE64_STANDARD.decode(b64).expect("base64 decode");
        assert_eq!(decoded.len(), 64, "decoded sig must be 64 bytes");

        // The persona body hash + the metadata signature must verify
        // against the curator's public key.
        let body_md: String = conn
            .query_row(
                "SELECT content FROM memories WHERE id = ?1",
                rusqlite::params![&persona.id],
                |r| r.get(0),
            )
            .unwrap();
        let mut hasher = Sha256::new();
        hasher.update(body_md.as_bytes());
        let mut body_hash = [0u8; 32];
        body_hash.copy_from_slice(&hasher.finalize());

        let signable = SignablePersona {
            persona_id: persona.id.as_str(),
            entity_id: persona.entity_id.as_str(),
            namespace: persona.namespace.as_str(),
            version: persona.version,
            generated_at: persona.generated_at.as_str(),
            sources: &persona.sources,
            body_md_sha256: &body_hash,
        };
        let bytes = crate::identity::sign::canonical_cbor_persona(&signable).unwrap();
        let mut arr = [0u8; 64];
        arr.copy_from_slice(&decoded);
        let sig = ed25519_dalek::Signature::from_bytes(&arr);
        use ed25519_dalek::Verifier;
        kp.public.verify(&bytes, &sig).expect("verify persona sig");
    }

    #[test]
    fn generate_signed_path_emits_signed_event() {
        // BUG-C — the `persona_generated` audit row must carry the
        // same signature bytes the metadata holds, and its
        // attest_level must agree.
        let (conn, _dir) = fresh_db();
        seed_two_alice_reflections(&conn, "team/alpha");
        let kp = crate::identity::keypair::generate("ai:curator").unwrap();
        let llm = StubLlm {
            canned: "body".into(),
        };
        let generator = PersonaGenerator::new(&conn, &llm, Some(&kp), PersonaConfig::default());
        let persona = generator.generate("alice", "team/alpha").expect("generate");

        let (sig, attest): (Option<Vec<u8>>, String) = conn
            .query_row(
                "SELECT signature, attest_level FROM signed_events \
                 WHERE event_type = 'persona_generated' \
                 ORDER BY sequence DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attest, "self_signed");
        let sig_bytes = sig.expect("signed audit row must have signature");
        assert_eq!(sig_bytes.len(), 64);

        // Cross-check vs the metadata's base64 signature.
        let meta_str: String = conn
            .query_row(
                "SELECT metadata FROM memories WHERE id = ?1",
                rusqlite::params![&persona.id],
                |r| r.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
        let b64 = meta["persona"]["signature"].as_str().unwrap();
        let decoded = BASE64_STANDARD.decode(b64).unwrap();
        assert_eq!(
            decoded, sig_bytes,
            "metadata signature must match signed_events.signature"
        );
    }

    #[test]
    fn generate_with_public_only_keypair_falls_back_to_unsigned() {
        // A public-only handle (can_sign() == false) must collapse
        // to the unsigned path: the substrate must not pretend to
        // sign with a key it cannot use.
        let (conn, _dir) = fresh_db();
        seed_two_alice_reflections(&conn, "team/alpha");
        let kp = crate::identity::keypair::generate("ai:curator").unwrap();
        let pub_only = crate::identity::keypair::AgentKeypair {
            agent_id: "ai:curator".to_string(),
            public: kp.public,
            private: None,
        };
        let llm = StubLlm {
            canned: "body".into(),
        };
        let generator =
            PersonaGenerator::new(&conn, &llm, Some(&pub_only), PersonaConfig::default());
        let persona = generator.generate("alice", "team/alpha").expect("generate");
        assert_eq!(
            persona.attest_level, "unsigned",
            "public-only keypair must NOT produce self_signed"
        );
        let meta_str: String = conn
            .query_row(
                "SELECT metadata FROM memories WHERE id = ?1",
                rusqlite::params![&persona.id],
                |r| r.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
        assert!(
            meta["persona"]["signature"].is_null()
                || !meta["persona"]
                    .as_object()
                    .unwrap()
                    .contains_key("signature"),
            "metadata must not carry a signature for the unsigned path"
        );
    }

    #[test]
    fn signed_persona_v2_regenerates_with_fresh_signature() {
        // BUG-B regression — calling generate() twice with the same
        // keypair MUST produce two distinct signed personas (different
        // ids, different signatures) and v2 must still be signed
        // end-to-end. This pins the "regeneration also signs" property.
        let (conn, _dir) = fresh_db();
        seed_two_alice_reflections(&conn, "team/alpha");
        let kp = crate::identity::keypair::generate("ai:curator").unwrap();
        let llm = StubLlm {
            canned: "body".into(),
        };
        let generator = PersonaGenerator::new(&conn, &llm, Some(&kp), PersonaConfig::default());
        let v1 = generator.generate("alice", "team/alpha").expect("v1");
        let v2 = generator.generate("alice", "team/alpha").expect("v2");
        assert_eq!(v1.version, 1);
        assert_eq!(v2.version, 2);
        assert_ne!(v1.id, v2.id);
        assert_eq!(v1.attest_level, "self_signed");
        assert_eq!(v2.attest_level, "self_signed");

        // Both v1 and v2 metadata envelopes carry a 64-byte sig and
        // they differ (different persona_id pins different bytes).
        let sigs: Vec<Vec<u8>> = [&v1.id, &v2.id]
            .iter()
            .map(|id| {
                let meta_str: String = conn
                    .query_row(
                        "SELECT metadata FROM memories WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get(0),
                    )
                    .unwrap();
                let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
                let b64 = meta["persona"]["signature"].as_str().expect("sig present");
                BASE64_STANDARD.decode(b64).unwrap()
            })
            .collect();
        assert_eq!(sigs[0].len(), 64);
        assert_eq!(sigs[1].len(), 64);
        assert_ne!(sigs[0], sigs[1], "v1 + v2 signatures must differ");
    }
}
