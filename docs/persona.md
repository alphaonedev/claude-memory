# Persona-as-artifact (v0.7.0 QW-2)

A **Persona** is a curator-generated Markdown profile of an entity,
synthesised from a cluster of `MemoryKind::Reflection` rows that
reference that entity. Personas are the substrate-native expression of
the Tencent L3 pattern (PersonaMem 48% → 76% on the long-horizon
benchmark): the substrate distils the agent's reflections about a
subject into a stable, recallable artefact so the agent can re-load
"what we know about Alice" with a single recall hit instead of paging
through dozens of disjoint reflection rows.

## What a Persona is

A first-class `MemoryKind::Persona` row in the `memories` table, with
two extra columns populated:

- `entity_id TEXT NULL` — the subject of the persona.
- `persona_version INTEGER NULL` — monotonic counter per
  `(entity_id, namespace)`. v1 on the first generation, v2 on the next,
  and so on. Old versions stay on disk for audit.

The `content` column carries a 300–500 word Markdown body. Every claim
in the body is footnoted with a `[^N]: <reflection-id>` citation so an
operator inspecting the row can follow the link back to the originating
reflection via `ai-memory get <id>`. The `metadata` column carries the
`persona` envelope:

```json
{
  "agent_id": "ai:curator",
  "persona": {
    "entity_id": "alice",
    "sources": ["<reflection-id>", "..."],
    "version": 1,
    "attest_level": "unsigned",
    "generated_at": "2026-05-15T00:00:00Z"
  }
}
```

The substrate writes one `derived_from` `memory_link` edge per source
reflection so the KG walker (`memory_find_paths`, `memory_kg_query`)
can follow the Persona → Reflection → Observation chain end-to-end.
Every generation also appends a `persona_generated` row to
`signed_events` with the sources hash as `payload_hash`; the H5 audit
chain captures every regeneration as a distinct, signed event.

## When to generate one

Three ways to trigger a generation:

1. **MCP write tool.** `memory_persona_generate({entity_id, namespace})`
   runs the curator synchronously and returns the new persona over the
   wire. Requires `--tier smart` or higher; the dispatcher refuses
   below.
2. **CLI.** `ai-memory persona <entity_id> --regenerate` calls the
   same path from the operator's shell.
3. **Namespace cadence.** Set
   `governance.auto_persona_trigger_every_n_memories = N` on the
   namespace standard and the substrate's post-reflect hook fires a
   deferred regeneration every N reflection writes that mention the
   entity. The hook is notify-class — failures are logged at
   `tracing::warn!(target: "post_reflect.auto_persona", ...)` and never
   propagated to the caller's reflect response.

## Provenance

The substrate is the source of truth. Every persona row carries
*verifiable* provenance:

- `entity_id` + `persona_version` on the SQL row (indexed via
  `idx_personas_by_entity`).
- One `derived_from` edge per source reflection. The reverse traversal
  (`SELECT source_id FROM memory_links WHERE target_id = ? AND
  relation = 'derived_from'`) returns the personas that derive from a
  given reflection — useful for the operator dashboard.
- One row in `signed_events` with `event_type = 'persona_generated'`,
  `payload_hash = SHA256(persona_id || source_ids)`. The H5 cross-row
  hash chain catches tampering at audit time.
- `metadata.persona.attest_level` summarises the strongest attestation
  across the `derived_from` edges; symmetric with QW-1's reflection
  export envelope.

## File-backed export

When the namespace policy carries
`governance.auto_export_personas_to_filesystem = true`, the substrate
writes
`~/.ai-memory/personas/<namespace-sanitised>/<entity_id>.md` after each
generation. The file is a YAML-frontmatter Markdown document containing
the same fields as the SQL row plus the rendered body. Operators may
freely delete or regenerate the directory — the SQL row stays
canonical.

The export is opt-in per namespace, symmetric with QW-1's
`auto_export_reflections_to_filesystem`. Operators who want governance
enforcement *without* plaintext personas on disk leave the policy
absent.

## Read-back paths

Three equivalent ways to read the latest persona:

```bash
# CLI — Markdown to stdout
ai-memory persona alice --namespace team/alpha

# CLI — JSON envelope to stdout
ai-memory persona alice --namespace team/alpha --json

# MCP — read-only tool, available at Semantic+
memory_persona({entity_id: "alice", namespace: "team/alpha"})
```

The CLI honors the namespace flag (`--namespace`, defaults to
`global`), the JSON toggle (`--json`), and the regeneration toggle
(`--regenerate`). The regenerate path requires an LLM client; the CLI
refuses with exit code 2 and a documented hint when none is wired (the
MCP path is the standard way to regenerate because the daemon already
owns the OllamaClient).

## Trade-offs

- **Curator dependence.** Personas only mint when the LLM trait is
  wired. The substrate's `AutonomyLlm` trait is satisfied by Ollama in
  production and by the in-process `MockOllamaClient` in tests; below
  the smart tier the MCP dispatcher refuses the write surface.
- **Quality is bounded by reflections.** Personas are distillations of
  reflections, not of raw observations. A namespace with no
  reflections (or no reflections mentioning the entity) yields
  `PersonaError::NoReflections` — the substrate refuses to mint a
  persona without an audit trail.
- **Regeneration is additive.** Each call writes a fresh row with
  `persona_version + 1`; the substrate never overwrites in place.
  Operators who want the old behaviour can delete prior rows
  explicitly via `memory_delete`.
- **Append-only KG growth.** Every regeneration adds N
  `derived_from` edges plus one `signed_events` row. Long-running
  namespaces with aggressive cadences will see KG growth proportional
  to (entities × cadence). The L2-3 invalidation walker (#668) treats
  Persona supersession the same way it treats Reflection supersession.

## Related primitives

- **QW-1** — file-backed reflection chain export. The Persona file
  exporter shares the same `<HOME>/.ai-memory/.../<name>.md` shape so
  operators can mix both directories under a single tree.
- **L2-1** — the reflection-pass curator that produces the Reflection
  rows Personas distil from. Personas presuppose L2-1's output.
- **L2-3** — Reflection invalidation propagation. A Persona that
  derives from a now-invalidated reflection inherits the dependency
  flag; the operator dashboard surfaces stale personas alongside
  stale reflections.
- **H5** — signed-events append-only chain. Every Persona generation
  lands one row here so an auditor can replay the chain.
