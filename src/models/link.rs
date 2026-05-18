// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// v0.7 Track H — attestation level for a `memory_links` row.
///
/// H2 (#566) and H3 (#572) already write the three string variants
/// directly into the `memory_links.attest_level` TEXT column
/// (`"unsigned"`, `"self_signed"`, `"peer_attested"`). H4 formalises
/// the enum so the `memory_verify` MCP tool — and any future verifier
/// surface — can reason in terms of a closed set rather than an
/// open-ended string.
///
/// `#[serde(rename_all = "snake_case")]` keeps the wire shape byte-
/// identical to what the database column already holds. The
/// [`AttestLevel::from_str`] / [`AttestLevel::as_str`] helpers exist
/// because the column is read as a `String` in many call sites that
/// are not deserialising through serde (e.g. `rusqlite::Row::get`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestLevel {
    /// No signature on the row, or no key enrolled for `observed_by` on
    /// the receiver. Federation back-compat default — unsigned rows
    /// still land but downstream consumers know they cannot verify.
    Unsigned,
    /// Row was signed locally by this writer (H2 outbound path).
    SelfSigned,
    /// Row arrived from a peer with a signature that verified against
    /// the enrolled `observed_by` public key on this host (H3 inbound
    /// path).
    PeerAttested,
}

impl AttestLevel {
    /// Parse the string form stored in `memory_links.attest_level`.
    ///
    /// Returns `None` for unknown values so callers can decide whether
    /// to treat the column as legacy/`unsigned` or surface an error.
    /// Keeps the unit-of-truth on the database column shape — H2/H3
    /// already write the canonical lowercase snake_case strings.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "unsigned" => Some(Self::Unsigned),
            "self_signed" => Some(Self::SelfSigned),
            "peer_attested" => Some(Self::PeerAttested),
            _ => None,
        }
    }

    /// Canonical wire string for this variant. Mirrors the `serde`
    /// rename_all and the literals H2/H3 already write to the DB.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unsigned => "unsigned",
            Self::SelfSigned => "self_signed",
            Self::PeerAttested => "peer_attested",
        }
    }
}

impl std::fmt::Display for AttestLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// v0.7.0 fix campaign R1-M4 — typed relation closed-set for
/// `memory_links.relation`. Paired with the SQL-side CHECK constraint
/// added by the same R1-M4 migration: defense-in-depth so direct-SQL
/// writers can no longer slip an unknown relation past the Rust
/// validator.
///
/// `#[serde(rename_all = "snake_case")]` keeps the wire shape and the
/// `memory_links.relation` TEXT column byte-identical to the values
/// the v0.6.x codebase already writes (`"related_to"`, `"supersedes"`,
/// `"contradicts"`, `"derived_from"`, `"reflects_on"`, plus the
/// v0.7.0 WT-1-A addition `"derives_from"` — distinct from
/// `"derived_from"` as the atomisation-provenance variant). The
/// [`MemoryLinkRelation::from_str`] / [`MemoryLinkRelation::as_str`]
/// helpers exist because the column is read as a `String` in many
/// call sites that are not deserialising through serde (e.g.
/// `rusqlite::Row::get`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLinkRelation {
    /// Generic association. Default for `LinkBody::resolved` and the
    /// `INSERT` default in the SQL schema.
    RelatedTo,
    /// Source supersedes target (newer / authoritative version).
    Supersedes,
    /// Source contradicts target (incompatible claims).
    Contradicts,
    /// Source is derived from target (consolidation provenance).
    DerivedFrom,
    /// Source is a reflection on target (recursive-learning provenance,
    /// v0.7.0 Task 1/8).
    ReflectsOn,
    /// Source is an atomisation derivative of target — the typed,
    /// signable, federation-safe expression of the structural
    /// `memories.atom_of` FK introduced in v0.7.0 WT-1-A (schema v36
    /// sqlite / v35 postgres). Atom row -> parent memory. Participates
    /// in `find_paths` traversal alongside the other relations.
    /// Distinct from `DerivedFrom` (consolidation provenance):
    /// atomisation is a finer-grained, recoverable split that emits
    /// one `derives_from` edge per atom; consolidation merges several
    /// memories into one and emits `derived_from` edges from the
    /// consolidated memory back to each source.
    DerivesFrom,
}

impl MemoryLinkRelation {
    /// Parse the string form stored in `memory_links.relation`.
    ///
    /// Returns `None` for unknown values so callers can decide whether
    /// to reject with a typed error or fall back to a default. The
    /// canonical strings are the SQL-side CHECK constraint membership
    /// list — keep this list in sync with the migration.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "related_to" => Some(Self::RelatedTo),
            "supersedes" => Some(Self::Supersedes),
            "contradicts" => Some(Self::Contradicts),
            "derived_from" => Some(Self::DerivedFrom),
            "reflects_on" => Some(Self::ReflectsOn),
            "derives_from" => Some(Self::DerivesFrom),
            _ => None,
        }
    }

    /// Canonical wire string for this variant. Mirrors the `serde`
    /// rename_all and the literals every existing call site already
    /// writes to the DB.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RelatedTo => "related_to",
            Self::Supersedes => "supersedes",
            Self::Contradicts => "contradicts",
            Self::DerivedFrom => "derived_from",
            Self::ReflectsOn => "reflects_on",
            Self::DerivesFrom => "derives_from",
        }
    }

    /// Canonical default — matches the `DEFAULT 'related_to'` clause
    /// on `memory_links.relation` in the schema and the fallback in
    /// `LinkBody::resolved`.
    #[must_use]
    pub const fn default_relation() -> Self {
        Self::RelatedTo
    }
}

impl std::fmt::Display for MemoryLinkRelation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for MemoryLinkRelation {
    fn default() -> Self {
        Self::default_relation()
    }
}

impl std::str::FromStr for MemoryLinkRelation {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str(s).ok_or_else(|| {
            format!(
                "invalid memory_link relation '{s}' (expected one of: related_to, \
                 supersedes, contradicts, derived_from, reflects_on, derives_from)"
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLink {
    pub source_id: String,
    pub target_id: String,
    /// v0.7.0 fix campaign R1-M4 — typed closed set. Round-trips with
    /// the `memory_links.relation` TEXT column via
    /// `MemoryLinkRelation::as_str` (write) / `from_str` (read). The
    /// SQL CHECK constraint added in migration 0023 enforces the same
    /// membership at the storage layer so direct-SQL writers cannot
    /// bypass the Rust validator.
    pub relation: MemoryLinkRelation,
    pub created_at: String,
    /// v0.7 H3 — optional 64-byte Ed25519 signature carried over the
    /// federation wire. `None` for legacy peers (pre-v0.7) that do not
    /// sign outbound links; receivers in that case land the row with
    /// `attest_level = "unsigned"`. When `Some`, it is verified against
    /// the public key associated with `observed_by` before insert.
    /// `skip_serializing_if` keeps the wire shape byte-identical to
    /// pre-H3 for unsigned rows so v0.6.x peers continue to deserialize
    /// without surprise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Vec<u8>>,
    /// v0.7 H3 — agent_id that asserts this link. Mirrors the H2
    /// `SignableLink.observed_by` field. Required when `signature` is
    /// `Some` (it is the lookup key for the verifying public key);
    /// `None` is treated as "no claim" and short-circuits to unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_by: Option<String>,
    /// v0.7 H3 — RFC3339 instant the link became true (matches the
    /// homonymous column in `memory_links`). Part of the signed bundle;
    /// must round-trip byte-identical with what the sender signed for
    /// verification to succeed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<String>,
    /// v0.7 H3 — RFC3339 instant the link was invalidated, or `None` if
    /// still valid. Part of the signed bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
    /// v0.7 H4 — attestation level for the row (`"unsigned"`,
    /// `"self_signed"`, `"peer_attested"`). Populated by readers that
    /// surface the `memory_links.attest_level` TEXT column (e.g.
    /// `db::get_links` for the `memory_get_links` MCP tool). Stays
    /// `None` on constructors that don't go through a DB read — those
    /// paths still feed `create_link_inbound` which derives the column
    /// value from the `attest_level: &str` parameter. The
    /// `skip_serializing_if` keeps the wire shape byte-identical to
    /// pre-v0.7 federation peers that don't carry the column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attest_level: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LinkBody {
    /// Canonical name. Aliased by `from` (S82's wire shape).
    #[serde(default)]
    pub source_id: Option<String>,
    /// `from` alias for `source_id`.
    #[serde(default)]
    pub from: Option<String>,
    /// Canonical name. Aliased by `to` (S82's wire shape).
    #[serde(default)]
    pub target_id: Option<String>,
    /// `to` alias for `target_id`.
    #[serde(default)]
    pub to: Option<String>,
    /// Canonical name. Aliased by `rel_type` (S82's wire shape).
    #[serde(default)]
    pub relation: Option<String>,
    /// `rel_type` alias for `relation`.
    #[serde(default)]
    pub rel_type: Option<String>,
}

impl LinkBody {
    /// Resolve the canonical (source_id, target_id, relation) tuple
    /// from the canonical fields or their aliases. Defaults relation
    /// to `related_to` when neither field is supplied.
    #[must_use]
    pub fn resolved(&self) -> (String, String, String) {
        let s = self
            .source_id
            .clone()
            .or_else(|| self.from.clone())
            .unwrap_or_default();
        let t = self
            .target_id
            .clone()
            .or_else(|| self.to.clone())
            .unwrap_or_default();
        let r = self
            .relation
            .clone()
            .or_else(|| self.rel_type.clone())
            .unwrap_or_else(default_relation);
        (s, t, r)
    }
}

fn default_relation() -> String {
    "related_to".to_string()
}

/// Tag stamped on entity-typed memories so `(title, namespace)` can be
/// shared across regular memories and entities without ambiguity (Pillar
/// 2 / Stream B).
pub const ENTITY_TAG: &str = "entity";

/// Marker written to `metadata.kind` on entity-typed memories. The
/// db layer keys entity lookups off this field so the alias resolver
/// never returns a regular memory that happens to share a title with an
/// entity registered later.
pub const ENTITY_KIND: &str = "entity";

/// Resolved entity record returned by `db::entity_get_by_alias` and
/// embedded in the `db::entity_register` response (Pillar 2 / Stream B).
/// `aliases` is the full alias set for the entity, ordered by
/// `created_at ASC, alias ASC` for stable display.
#[derive(Debug, Clone, Serialize)]
pub struct EntityRecord {
    pub entity_id: String,
    pub canonical_name: String,
    pub namespace: String,
    pub aliases: Vec<String>,
}

/// Outcome of `db::entity_register`. `created` is `true` when a new
/// entity memory was inserted, `false` when an existing entity was
/// reused (idempotent re-registration that just merged new aliases into
/// the existing record).
#[derive(Debug, Clone, Serialize)]
pub struct EntityRegistration {
    pub entity_id: String,
    pub canonical_name: String,
    pub namespace: String,
    pub aliases: Vec<String>,
    pub created: bool,
}

/// Single row returned by `db::kg_timeline` (Pillar 2 / Stream C).
///
/// Captures one outbound assertion from a source memory: the
/// `target_id` and its `relation`, the temporal-validity window
/// (`valid_from` / `valid_until`), the agent that observed it
/// (`observed_by`), and the target's display fields (`title`,
/// `target_namespace`) for caller convenience. `valid_from` is the
/// authoritative ordering key — events with NULL `valid_from` are
/// excluded from the timeline by the query.
#[derive(Debug, Clone, Serialize)]
pub struct KgTimelineEvent {
    pub target_id: String,
    pub relation: String,
    pub valid_from: String,
    pub valid_until: Option<String>,
    pub observed_by: Option<String>,
    pub title: String,
    pub target_namespace: String,
}

/// One node returned by `db::kg_query` (Pillar 2 / Stream C —
/// `memory_kg_query`). Each node represents a memory reachable from the
/// query's source through one outbound link, carrying the link's
/// temporal-validity columns plus the target memory's display fields and
/// the traversal path. `depth` is the actual number of hops from the
/// source (1..=`KG_QUERY_MAX_SUPPORTED_DEPTH`); `path` is the
/// `src->mid->target` chain as discovered by the recursive CTE.
#[derive(Debug, Clone, Serialize)]
pub struct KgQueryNode {
    pub target_id: String,
    pub relation: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub observed_by: Option<String>,
    pub title: String,
    pub target_namespace: String,
    pub depth: usize,
    pub path: String,
}

/// One nearest-neighbor result from a `memory_check_duplicate` lookup
/// (Pillar 2 / Stream D). `similarity` is the cosine similarity in
/// `[-1.0, 1.0]`, rounded to three decimals at the response layer.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateMatch {
    pub id: String,
    pub title: String,
    pub namespace: String,
    pub similarity: f32,
}

/// Result envelope returned by `db::check_duplicate`.
///
/// `is_duplicate` is `nearest.similarity >= threshold`. `nearest` is
/// `None` only when the candidate pool is empty (no embedded, live
/// memories matched the namespace filter). When `is_duplicate` is true,
/// `nearest.id` doubles as the suggested merge target — we surface it
/// under that name in the JSON response so the contract stays explicit.
#[derive(Debug, Clone, Serialize)]
pub struct DuplicateCheck {
    pub is_duplicate: bool,
    pub threshold: f32,
    pub nearest: Option<DuplicateMatch>,
    pub candidates_scanned: usize,
}

/// One node of the hierarchical namespace tree returned by
/// `memory_get_taxonomy` (Pillar 1 / Stream A).
///
/// `count` is the number of memories at *exactly* this namespace;
/// `subtree_count` is the count of memories at this node plus every
/// descendant the depth limit allowed us to expand. Children are sorted
/// alphabetically by `name` so callers get a stable rendering order.
#[derive(Debug, Clone, Serialize)]
pub struct TaxonomyNode {
    /// Full namespace path of this node. Empty string for the synthetic
    /// root when no `namespace_prefix` is supplied.
    pub namespace: String,
    /// Last `/`-delimited segment of `namespace` (display label). Empty
    /// for the synthetic root.
    pub name: String,
    /// Memories whose namespace equals this node's `namespace`.
    pub count: usize,
    /// Memories at this node plus all descendants visible within the
    /// requested `depth`. Memories beneath the depth cutoff still
    /// contribute to the `subtree_count` of the boundary ancestor.
    pub subtree_count: usize,
    /// Direct child nodes, sorted alphabetically by `name`.
    pub children: Vec<TaxonomyNode>,
}

/// Result envelope returned by `db::get_taxonomy`.
///
/// `total_count` is the global memory count for the prefix (independent
/// of `depth`/`limit` truncation) so callers can render an honest
/// "X memories in N namespaces" header even when the tree was
/// truncated. `truncated` is set when the `limit` parameter forced us
/// to drop input rows when assembling the tree.
#[derive(Debug, Clone, Serialize)]
pub struct Taxonomy {
    pub tree: TaxonomyNode,
    pub total_count: usize,
    pub truncated: bool,
}

/// Phase 3 foundation (issue #224): vector clock tracking the latest
/// `updated_at` this peer has seen from each known remote peer.
///
/// Entries are populated lazily — both on HTTP `/sync/push` (receiver
/// records the sender's latest `updated_at`) and on HTTP `/sync/since`
/// (sender advances `last_pulled_at`). Full CRDT-lite merge rules using
/// the clock are **not** in the v0.6.0 GA foundation; they land in a
/// follow-up PR under issue #224 Task 3a.1. The foundation ships the
/// wire format so adding the merge semantics later does not force a
/// schema migration.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VectorClock {
    /// Map of peer `agent_id` -> latest RFC3339 `updated_at` seen from
    /// that peer. A peer absent from the map is equivalent to
    /// "never-seen-anything." Encoded as a JSON object on the wire.
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, String>,
}

impl VectorClock {
    /// Advance this clock to include `peer_id`'s latest seen timestamp.
    /// Monotonic — an older timestamp never overwrites a newer one.
    #[allow(dead_code)] // Consumed by Task 3a.1 CRDT-lite merge (issue #224).
    pub fn observe(&mut self, peer_id: &str, at: &str) {
        self.entries
            .entry(peer_id.to_string())
            .and_modify(|existing| {
                if at > existing.as_str() {
                    *existing = at.to_string();
                }
            })
            .or_insert_with(|| at.to_string());
    }

    /// Look up the latest timestamp this clock has from `peer_id`.
    #[must_use]
    #[allow(dead_code)] // Consumed by Task 3a.1 CRDT-lite merge (issue #224).
    pub fn latest_from(&self, peer_id: &str) -> Option<&str> {
        self.entries.get(peer_id).map(String::as_str)
    }
}

/// Phase 3 foundation: one row of the `sync_state` table serialised for
/// diagnostic / API responses.
#[allow(dead_code)] // Consumed by Task 3b.2 sync diagnostics API (issue #224).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStateEntry {
    pub agent_id: String,
    pub peer_id: String,
    pub last_seen_at: String,
    pub last_pulled_at: String,
}

// -----------------------------------------------------------------
// L0.7-2 Tier A — LinkBody alias + AttestLevel + VectorClock coverage
// -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn parse_link_body(json: serde_json::Value) -> LinkBody {
        serde_json::from_value(json).expect("LinkBody deserialises")
    }

    #[test]
    fn link_body_resolved_uses_canonical_fields_when_present() {
        let b = parse_link_body(serde_json::json!({
            "source_id": "src",
            "target_id": "tgt",
            "relation": "supersedes",
        }));
        let (s, t, r) = b.resolved();
        assert_eq!(s, "src");
        assert_eq!(t, "tgt");
        assert_eq!(r, "supersedes");
    }

    #[test]
    fn link_body_resolved_falls_back_to_from_alias() {
        // Line 135: from-alias path for source_id
        let b = parse_link_body(serde_json::json!({
            "from": "from-id",
            "to": "to-id",
            "rel_type": "contradicts",
        }));
        let (s, t, r) = b.resolved();
        assert_eq!(s, "from-id");
        assert_eq!(t, "to-id");
        assert_eq!(r, "contradicts");
    }

    #[test]
    fn link_body_resolved_defaults_relation_to_related_to() {
        // Lines 145, 151-153: default_relation invoked when neither
        // `relation` nor `rel_type` set.
        let b = parse_link_body(serde_json::json!({
            "source_id": "a",
            "target_id": "b",
        }));
        let (_s, _t, r) = b.resolved();
        assert_eq!(r, "related_to");
    }

    #[test]
    fn link_body_resolved_empty_payload_returns_empty_strings_and_default() {
        let b = parse_link_body(serde_json::json!({}));
        let (s, t, r) = b.resolved();
        assert_eq!(s, "");
        assert_eq!(t, "");
        assert_eq!(r, "related_to");
    }

    #[test]
    fn link_body_resolved_canonical_wins_over_alias() {
        // When BOTH canonical and alias are set, the canonical wins.
        let b = parse_link_body(serde_json::json!({
            "source_id": "canonical-src",
            "from": "alias-src",
            "target_id": "canonical-tgt",
            "to": "alias-tgt",
            "relation": "canonical-rel",
            "rel_type": "alias-rel",
        }));
        let (s, t, r) = b.resolved();
        assert_eq!(s, "canonical-src");
        assert_eq!(t, "canonical-tgt");
        assert_eq!(r, "canonical-rel");
    }

    #[test]
    fn attest_level_round_trips_strings() {
        for (s, v) in [
            ("unsigned", AttestLevel::Unsigned),
            ("self_signed", AttestLevel::SelfSigned),
            ("peer_attested", AttestLevel::PeerAttested),
        ] {
            assert_eq!(AttestLevel::from_str(s), Some(v));
            assert_eq!(v.as_str(), s);
            assert_eq!(format!("{v}"), s);
        }
    }

    #[test]
    fn attest_level_from_str_returns_none_for_unknown() {
        assert_eq!(AttestLevel::from_str("unknown"), None);
        assert_eq!(AttestLevel::from_str(""), None);
    }

    #[test]
    fn vector_clock_observe_advances_monotonically() {
        let mut c = VectorClock::default();
        c.observe("peer-a", "2026-01-01T00:00:00Z");
        assert_eq!(c.latest_from("peer-a"), Some("2026-01-01T00:00:00Z"));
        // Later timestamp must replace.
        c.observe("peer-a", "2026-02-01T00:00:00Z");
        assert_eq!(c.latest_from("peer-a"), Some("2026-02-01T00:00:00Z"));
        // Earlier timestamp must NOT replace.
        c.observe("peer-a", "2025-12-01T00:00:00Z");
        assert_eq!(c.latest_from("peer-a"), Some("2026-02-01T00:00:00Z"));
    }

    #[test]
    fn vector_clock_latest_from_unknown_peer_is_none() {
        let c = VectorClock::default();
        assert_eq!(c.latest_from("never-seen"), None);
    }

    #[test]
    fn vector_clock_serializes_as_object_with_entries() {
        let mut c = VectorClock::default();
        c.observe("peer-a", "2026-01-01T00:00:00Z");
        let json = serde_json::to_value(&c).unwrap();
        assert!(json.get("entries").is_some());
        assert_eq!(
            json["entries"]["peer-a"],
            serde_json::Value::String("2026-01-01T00:00:00Z".to_string())
        );
    }

    // ---- C-5 (#699): lift coverage on MemoryLinkRelation parsing/defaults.
    // Targets uncovered: `MemoryLinkRelation::from_str` unknown branch,
    // `default_relation`, `Default::default`, `FromStr` wrapper. ----

    #[test]
    fn memory_link_relation_from_str_returns_none_for_unknown() {
        // Line 116: `_ => None` arm of the inherent from_str.
        assert_eq!(MemoryLinkRelation::from_str("bogus"), None);
        assert_eq!(MemoryLinkRelation::from_str(""), None);
        assert_eq!(MemoryLinkRelation::from_str("RELATED_TO"), None);
    }

    #[test]
    fn memory_link_relation_default_relation_is_related_to() {
        // Lines 138-140: `default_relation()` associated function.
        let d = MemoryLinkRelation::default_relation();
        assert_eq!(d, MemoryLinkRelation::RelatedTo);
        assert_eq!(d.as_str(), "related_to");
    }

    #[test]
    fn memory_link_relation_default_trait_uses_related_to() {
        // Lines 150-152: `Default::default()` implementation.
        let d: MemoryLinkRelation = Default::default();
        assert_eq!(d, MemoryLinkRelation::RelatedTo);
    }

    #[test]
    fn memory_link_relation_from_str_trait_round_trips_canonical_strings() {
        // Lines 158-165: `std::str::FromStr::from_str` wrapper.
        for (s, v) in [
            ("related_to", MemoryLinkRelation::RelatedTo),
            ("supersedes", MemoryLinkRelation::Supersedes),
            ("contradicts", MemoryLinkRelation::Contradicts),
            ("derived_from", MemoryLinkRelation::DerivedFrom),
            ("reflects_on", MemoryLinkRelation::ReflectsOn),
            ("derives_from", MemoryLinkRelation::DerivesFrom),
        ] {
            // Disambiguate against the inherent `from_str` (which returns
            // Option) by going through the `FromStr` trait fully qualified.
            let parsed: MemoryLinkRelation =
                <MemoryLinkRelation as std::str::FromStr>::from_str(s).unwrap();
            assert_eq!(parsed, v);
            // Display impl round-trip.
            assert_eq!(format!("{v}"), s);
        }
    }

    #[test]
    fn memory_link_relation_from_str_trait_returns_helpful_error_for_unknown() {
        // Lines 158-165: error arm of the FromStr wrapper.
        let err = <MemoryLinkRelation as std::str::FromStr>::from_str("nope").unwrap_err();
        assert!(err.contains("nope"));
        assert!(err.contains("related_to"));
        assert!(err.contains("reflects_on"));
    }
}
