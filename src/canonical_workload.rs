// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Schema regression tests for `benchmarks/v063/canonical_workload.json`.
//!
//! The canonical workload is a 1000-memory deterministic seed consumed by
//! the v0.6.3 curator-cycle bench (charter §"Stream E — Performance
//! Instrumentation"; budget published in `PERFORMANCE.md` as
//! `curator cycle (1k memories) < 60 s p95`). The fixture lands ahead of
//! its bench wiring; this module pins the on-disk shape so that future
//! edits to either the JSON or its Python generator do not silently
//! invalidate the curator-eligibility invariants the bench relies on.
//!
//! The fixture is loaded at test runtime (not `include_str!`-embedded)
//! so the production binary stays unaffected.

#[cfg(test)]
mod tests {
    use crate::curator::MIN_CONTENT_LEN;
    use crate::models::CreateMemory;
    use serde::Deserialize;
    use std::path::PathBuf;

    #[derive(Debug, Deserialize)]
    struct Workload {
        schema_version: u32,
        seed: u64,
        count: usize,
        memories: Vec<CreateMemory>,
    }

    fn load() -> Workload {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("benchmarks")
            .join("v063")
            .join("canonical_workload.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("deserialize {}: {e}", path.display()))
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(load().schema_version, 1);
    }

    #[test]
    fn seed_and_count_match_published_values() {
        let w = load();
        assert_eq!(w.seed, 20_260_426);
        assert_eq!(w.count, 1000);
        assert_eq!(w.memories.len(), 1000);
    }

    #[test]
    fn deserialises_into_create_memory() {
        // Round-trip via CreateMemory ensures the on-disk shape stays
        // compatible with the upsert path the bench wiring will call.
        let w = load();
        assert!(!w.memories.is_empty());
    }

    #[test]
    fn every_memory_is_curator_eligible() {
        // Curator's needs_curation() requires:
        //   - namespace not starting with '_'
        //   - content.len() >= MIN_CONTENT_LEN
        //   - tier == mid | long
        //   - no metadata.auto_tags
        // If any of these drift we silently lose curator-cycle bench coverage.
        let w = load();
        for (idx, m) in w.memories.iter().enumerate() {
            assert!(
                !m.namespace.starts_with('_'),
                "memory[{idx}] namespace `{}` starts with underscore — curator would skip",
                m.namespace,
            );
            assert!(
                m.content.len() >= MIN_CONTENT_LEN,
                "memory[{idx}] content len {} < MIN_CONTENT_LEN {}",
                m.content.len(),
                MIN_CONTENT_LEN,
            );
            let tier_str = m.tier.as_str();
            assert!(
                matches!(tier_str, "mid" | "long"),
                "memory[{idx}] tier `{tier_str}` not mid|long — curator scans neither",
            );
            let auto_tags = m
                .metadata
                .get("auto_tags")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty());
            assert!(
                !auto_tags,
                "memory[{idx}] already carries auto_tags — curator would short-circuit",
            );
        }
    }
}
