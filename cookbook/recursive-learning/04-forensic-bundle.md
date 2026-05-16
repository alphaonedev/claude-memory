# Recipe 04 — forensic bundle export + tamper detection

**Script:** [`04-forensic-bundle.sh`](04-forensic-bundle.sh)

## What this recipe proves

The procurement-grade forensic bundle (L2-5,
[#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670)) is the
OSS surface for the AgenticMem Attest tier:

> Given any reflection-bearing memory id, `ai-memory
> export-forensic-bundle` writes a **deterministic POSIX-ustar tarball**
> containing the target memory, every reachable source via
> `reflects_on` edges (when `--include-reflections` is passed), the
> manifest with per-file SHA-256 hashes and an optional Ed25519
> signature, signed-event envelopes, and an embedded auditor-friendly
> `verification.json` mirror. An external auditor — with the bundle
> file and the chain's signers' public keys, no live ai-memory
> deployment — can re-verify the entire chain via
> `ai-memory verify-forensic-bundle <bundle.tar>`. A single tampered
> byte makes verification refuse.

This recipe reproduces both halves of the contract: the clean bundle
verifies (exit 0, "verification OK"), and a one-byte tamper in a memory
body makes the verifier refuse (exit 1, "verification FAILED" plus a
`tampered_files` entry in the structured report).

## Why it matters

A chain of signed `reflects_on` edges inside the source DB is
audit-grade only as long as the source DB exists. The forensic bundle
is the **transport-layer** evidence package: it lifts the cryptographic
guarantees out of the substrate's storage and into a file an auditor
can verify offline. This is how AgenticMem Attest customers will hand
evidence to regulators — and the OSS verifier is the public
counter-check that the proprietary tier's bundles aren't laundered.

## What it does step by step

1. **Bootstrap.** Fresh sqlite under `.local-runs/cookbook-04-<ts>/memory.db`.
2. **Seed + chain.** Three depth-0 observations; one depth-1
   reflection over them; one depth-2 reflection on the depth-1 result.
3. **Export bundle.** Calls
   `ai-memory export-forensic-bundle --memory-id <depth-2-id>
   --include-reflections --output <bundle.tar>`. Asserts the bundle
   file exists; logs its size.
4. **Verify clean.** Calls
   `ai-memory verify-forensic-bundle <bundle.tar>`. Asserts exit code
   `0` AND the literal string `verification OK` on stdout.
5. **Tamper.** Locates the literal substring `memory_kind` inside the
   tarball (a marker that appears in every memory envelope past the
   manifest region) and flips the byte at that offset by `+1`. This
   sits well inside a memory body — not a ustar header — so the parser
   still reads the tar but the file's hash no longer matches the
   manifest's recorded `sha256`.
6. **Verify tampered.** Re-runs `verify-forensic-bundle` on the
   tampered copy. Asserts exit code is non-zero AND the literal string
   `verification FAILED` appears on stdout.
7. **Verdict.** Prints offsets and exit codes; exits 0 only when both
   the clean verification passed AND the tampered verification was
   refused.

## Expected output (abridged)

```
==> 3/6  export-forensic-bundle for depth-2 chain → …/forensic-bundle.tar
    bundle written (16384 bytes)
==> 4/6  verify-forensic-bundle (clean copy)
    clean bundle verified (exit 0, 'verification OK' on stdout) OK
==> 5/6  tamper one byte and re-verify (refusal expected)
    flipped byte at offset 7908 (109 → 110)
    verify exit code on tampered bundle: 1
    tamper detected: verify exited non-zero AND wrote 'verification FAILED' OK
```

## Acceptance contract

Exits `0` if and only if:

- The bundle file is written by `export-forensic-bundle` to the
  expected path and is non-empty.
- `verify-forensic-bundle` on the clean bundle returns exit 0 AND
  emits the literal line `verification OK`.
- `verify-forensic-bundle` on the tampered bundle returns a non-zero
  exit code AND emits the literal line `verification FAILED`.

## Cross-references

- Issue [#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670)
  — L2-5 forensic bundle export + verify.
- [`src/forensic/bundle.rs`](../../src/forensic/bundle.rs) — the
  substrate module. `verify` (line 685) is the verification entry
  point; `run_verify` (line 863) is the thin CLI dispatcher.
- [`docs/v0.7.0/release-notes.md`](../../docs/v0.7.0/release-notes.md)
  §"Procurement-grade forensic evidence".
- Recipe [`01-bounded-recursive-refinement.md`](01-bounded-recursive-refinement.md)
  — produces the same shape of chain this recipe bundles.

## Troubleshooting

- **"bundle is missing manifest.json"** — the tamper hit a ustar
  header region rather than a file body, breaking the tar parser. The
  recipe's marker-based offset finder should avoid this; if you
  manually adjust the offset, ensure it lands in a file body (the
  marker substring "memory_kind" is a reliable signpost since it
  appears in every memory envelope).
- **"tamper NOT detected"** — the byte flip is a no-op (e.g., the
  byte chosen was identical pre- and post-flip on some encoding
  boundary). The script uses `(byte + 1) & 0xff` which is always a
  distinct value; if you see this, the most likely cause is a
  binary that pre-dates L2-5 — rebuild from `feat/v0.7.0-grand-slam`
  at or after `c359e89`.
- **`verify-forensic-bundle` emits `UnknownSigner`** — the
  curator/agent that signed the chain has its public key registered
  on the source DB but not on the auditor's enrolled-peers list. For
  the cookbook this is a non-issue because the same DB serves as both
  source and auditor; in production the auditor must import the
  signer's `agent_id` + public key via
  `ai-memory identity import` before running verify.

## Offline auditor pattern

The bundle is self-contained but verification needs the chain's
signers' public keys. The intended workflow is:

1. Operator runs `ai-memory export-forensic-bundle …` on the source
   deployment.
2. Operator separately exports the relevant signer public keys via
   `ai-memory identity export-pub <agent-id>` — typically the curator's
   key plus any agents that produced source observations.
3. The bundle + the public keys ship to the auditor.
4. The auditor runs `ai-memory identity import <pubkey>` for each, then
   `ai-memory verify-forensic-bundle <bundle.tar>` — no source DB
   required. This recipe simulates that workflow in a single process;
   in production the two halves run on different machines and the
   bundle never carries private key material.
