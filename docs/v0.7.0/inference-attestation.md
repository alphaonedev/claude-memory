# Inference Backend — Supply-Chain Attestation Plan

**GH issue:** #654 (Strategic IP: distilled hot-path model + attested
model-weight supply chain).
**Related:** #651 (RFC pluggable inference backend trait — GPU),
#846 Gap #10 (MTP not live on current models).
**Operator directive:** `28860423-d12c-4959-bc8b-8fa9a94a33d9`
(2026-05-18) — no deferrals; v0.8 work pulled forward into v0.7.0
at minimum-viable scope.

## v0.7.0 MVP — landed

The substrate ships two attested-weights primitives at v0.7.0
(`src/inference/mod.rs`):

1. **`compute_attested_weights(path, label, signature) ->
   AttestedWeights`** — reads the on-disk weight file, computes the
   SHA-256, and returns an [`AttestedWeights`] envelope with optional
   Ed25519 signature.
2. **`verify_attested_weights(path, &expected) -> Result<()>`** —
   refuses if the file's recomputed SHA-256 does not match the
   `expected.sha256` value.

A backend implements [`InferenceBackend::attested_weights()`] to
surface the bound record; `CpuBackend::with_attested_weights(...)`
pins it explicitly. The `GpuBackend` stub returns `None` (no weights
loaded yet at v0.7.0 — issue #651 Phase 1 work).

Regression tests pin the round-trip + tampered-file refusal in
`src/inference/mod.rs` tests:

- `compute_and_verify_attested_weights_round_trip`
- `cpu_backend_with_attested_weights_round_trip`

## v0.7.0 MVP — out of scope

These are deliberately deferred to v0.8 because the surface they
touch is wider than the MVP gate:

1. **Sigstore / cosign bundle integration** — full transparency-log
   chain with rekor witness + fulcio cert. The MVP envelope leaves
   a `signature` slot but does not yet wire a verifier.
2. **Model-card / model-bom emission** — alongside the hash, emit a
   model-card YAML pinning training-set hash, framework version,
   tokenizer version, etc.
3. **Per-recall-path attestation telemetry** — emit
   `inference.attested_weights_verified=true` per recall so the
   audit trail can prove every served result came from a verified
   weight file.
4. **Key-rotation runbook** — see #846 Gap #9. The MVP shape carries
   `signature` as an opaque string; v0.8 will add `signed_by_key_id`
   so old signatures verify against archived public keys after a
   rotation.

## v0.8 work plan

| Phase | Work | LOE | Depends on |
|------:|---|---|---|
| Phase 1 (#651) | mistralrs or candle in-process GPU backend — replace `GpuBackend` stub with a real implementation | 2-3 wk | ML toolchain bring-up |
| Phase 2 (#654) | Sigstore bundle integration — verify `signature` against rekor + fulcio | 1-2 wk | Phase 1 |
| Phase 3 (#846 Gap 9) | Key-rotation runbook + `signed_by_key_id` schema column | 1-2 wk | Phase 2 |
| Phase 4 (#846 Gap 10) | MTP on-device speedup + per-recall attestation telemetry | 1 wk | Phases 1+2 |

## Operator gating

Per the v0.7.0 release gate (#836), inference attestation does NOT
block ship: it lands as a substrate primitive that is callable from
day one, with stubs for the GPU and signature surfaces. v0.8 closes
the rest of the supply-chain story.

## Provenance

- Operator directive: `28860423-d12c-4959-bc8b-8fa9a94a33d9`
- Strategic IP context memory: `338278f5-1d42-4e95-88c5-84d5fc3b1f53`
- Triage: `.local-runs/issue-triage-2026-05-18.md`
