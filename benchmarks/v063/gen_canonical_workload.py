#!/usr/bin/env python3
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""
Generate the v0.6.3 canonical workload fixture
(`benchmarks/v063/canonical_workload.json`).

The output is a deterministic 1000-memory seed that the curator-cycle
bench consumes to time one full curator sweep against the 60 s p95
budget published in `PERFORMANCE.md`. The generator is committed alongside
its output so the fixture is reproducible: re-running this script with no
arguments must produce a byte-identical JSON file.

Schema (top-level JSON object):

    {
      "schema_version": 1,
      "description": "...",
      "seed": 20260426,
      "count": 1000,
      "memories": [
        {
          "tier": "mid" | "long",
          "namespace": "projects/alpha/decisions",
          "title": "...",
          "content": "...",   # always >= curator MIN_CONTENT_LEN (50 chars)
          "tags": ["..."],
          "priority": 1..10,
          "confidence": 0.0..1.0,
          "source": "import",
          "metadata": {}      # empty so curator.needs_curation() is true
        },
        ...
      ]
    }

The shape lines up 1:1 with `crate::models::CreateMemory` so bench
wiring can `serde_json::from_str` the file directly. The fixture is
deliberately curator-eligible: every memory is in a public hierarchical
namespace, content >= 50 chars, no auto_tags metadata, mid/long tier
only — so a curator sweep finds 1000 candidates and exercises the
LLM-bound auto_tag + detect_contradiction loop until `max_ops_per_cycle`
is hit.

Usage:

    cd benchmarks/v063
    python3 gen_canonical_workload.py
    # writes canonical_workload.json next to this script
"""

from __future__ import annotations

import json
import random
from pathlib import Path

SCHEMA_VERSION = 1
SEED = 20260426  # v0.6.3 ship-target month — bump if the seed changes
COUNT = 1000

NAMESPACES = [
    "projects/alpha/decisions",
    "projects/alpha/meetings",
    "projects/alpha/code",
    "projects/alpha/research",
    "projects/beta/decisions",
    "projects/beta/meetings",
    "projects/beta/code",
    "projects/gamma/decisions",
    "projects/gamma/meetings",
    "clients/acme-corp/contracts",
    "clients/acme-corp/conversations",
    "clients/globex/contracts",
    "clients/globex/conversations",
    "ops/runbooks",
    "ops/postmortems",
    "ops/oncall",
    "personal/notes",
    "personal/reading",
    "research/papers",
    "research/datasets",
]

TAG_POOL = [
    "decision",
    "meeting",
    "code",
    "design",
    "review",
    "blocker",
    "follow-up",
    "open-question",
    "spike",
    "rfc",
    "incident",
    "postmortem",
    "deploy",
    "migration",
    "security",
    "perf",
    "doc",
    "client",
    "vendor",
    "internal",
]

# Content templates that produce >= 50 chars after substitution. The curator
# needs deterministic but varied content so detect_contradiction has at least
# *some* signal between adjacent memories in the same namespace.
TEMPLATES = [
    "Decision: {topic}. Owner: {owner}. Rationale: {rationale}. Next step: {next}.",
    "Meeting on {date} with {attendees}. Outcome: {outcome}. Action items: {actions}.",
    "Code review of {component} — feedback: {feedback}. Status: {status}.",
    "Design note for {component}. Constraint: {constraint}. Approach: {approach}.",
    "Postmortem: {incident}. Root cause: {cause}. Mitigation: {mitigation}.",
    "Runbook entry — {topic}. Trigger: {trigger}. Steps: {steps}.",
    "Research note on {topic}. Source: {source}. Takeaway: {takeaway}.",
    "Client {client} discussed {topic}. Outcome: {outcome}. Follow-up: {next}.",
    "Spike on {topic}. Hypothesis: {hypothesis}. Result: {result}. Next: {next}.",
    "Migration note — {component}. Before: {before}. After: {after}. Risk: {risk}.",
]

VOCAB = {
    "topic": [
        "vector index sizing",
        "schema migration cadence",
        "namespace hierarchy rollout",
        "embedder warmup",
        "FTS5 trigram tuning",
        "curator backlog growth",
        "federation ack budget",
        "agent-id rotation",
        "TTL relaxation policy",
        "subscription replay",
    ],
    "owner": [
        "alice",
        "bob",
        "carol",
        "dave",
        "erin",
        "frank",
        "grace",
        "heidi",
    ],
    "rationale": [
        "lowest-risk path forward",
        "best p95 trade-off",
        "operator feedback unanimous",
        "compatible with v0.7 plan",
        "smallest blast radius",
    ],
    "next": [
        "open PR by EOW",
        "draft RFC for review",
        "schedule follow-up sync",
        "wire into bench harness",
        "add migration step",
    ],
    "date": [
        "2026-04-08",
        "2026-04-15",
        "2026-04-22",
        "2026-04-29",
        "2026-05-06",
        "2026-05-13",
    ],
    "attendees": [
        "alice + bob",
        "alice + carol + dave",
        "engineering leads",
        "PM + tech lead",
        "alice + erin + frank",
    ],
    "outcome": [
        "agreed on plan",
        "deferred to next iteration",
        "blocked on dependency",
        "approved with caveats",
        "needs more data",
    ],
    "actions": [
        "alice files issue, bob drafts spec",
        "carol benchmarks fixture",
        "dave updates runbook",
        "erin writes RFC",
        "frank verifies CI",
    ],
    "component": [
        "memory_recall",
        "memory_kg_query",
        "curator daemon",
        "bench harness",
        "HTTP handlers",
        "MCP server loop",
        "embedder module",
    ],
    "feedback": [
        "minor — naming nit",
        "needs additional test",
        "shape looks right",
        "rebase before merge",
        "add a regression case",
    ],
    "status": [
        "approved",
        "changes requested",
        "merged",
        "blocked",
        "draft",
    ],
    "constraint": [
        "p95 < 100 ms on M4",
        "no new dependencies",
        "must round-trip SQLite ↔ Postgres",
        "backwards compatible with v0.6.2",
        "zero clippy::pedantic warnings",
    ],
    "approach": [
        "incremental ALTER + index",
        "behind a feature flag",
        "opt-in CLI flag",
        "schema bump with backfill",
        "two-phase rollout",
    ],
    "incident": [
        "FTS index drift",
        "embedder OOM on long input",
        "curator runaway on stuck LLM",
        "federation quorum stall",
        "session_start latency regression",
    ],
    "cause": [
        "missing index on temporal columns",
        "input not truncated before tokenize",
        "LLM client lacked timeout",
        "ack window mis-configured",
        "warm cache invalidated on hot reload",
    ],
    "mitigation": [
        "added index + backfill",
        "truncate at 8k tokens",
        "wrap LLM call in deadline",
        "raise W=2 timeout to 5 s",
        "preload warm cache during startup",
    ],
    "trigger": [
        "alert: p95 over budget",
        "ops on-call ping",
        "user-reported slow start",
        "CI bench regression",
        "log spike in errors",
    ],
    "steps": [
        "check journal, restart daemon, file ticket",
        "drain queue, replay subscriptions, verify",
        "rotate agent-id, refresh tokens, alert ops",
        "snapshot state, rollback, root-cause",
        "scale read replica, monitor for 30 min",
    ],
    "source": [
        "internal RFC",
        "external blog",
        "conference talk",
        "academic paper",
        "vendor docs",
    ],
    "takeaway": [
        "supports our current plan",
        "argues for opt-in default",
        "raises a soak-test concern",
        "validates the schema choice",
        "reinforces v0.7 sequencing",
    ],
    "client": [
        "acme-corp",
        "globex",
        "initech",
        "umbrella",
        "soylent",
    ],
    "hypothesis": [
        "embed call dominates p95",
        "FTS5 dominates p99",
        "curator stalls on long content",
        "HNSW build cost is amortizable",
        "rerank adds <5 ms",
    ],
    "result": [
        "confirmed — embed is 80% of latency",
        "rejected — FTS5 within budget",
        "partial — only stalls > 8k chars",
        "confirmed — amortizable across cycles",
        "confirmed — rerank within budget",
    ],
    "before": [
        "flat namespace, no hierarchy",
        "no temporal columns on links",
        "synchronous-only embedder",
        "single curator interval",
        "single-tier TTL",
    ],
    "after": [
        "namespace tree with /-delimited paths",
        "valid_from / valid_until on every link",
        "background embedder pool",
        "configurable curator interval per tier",
        "per-tier TTL with promotion",
    ],
    "risk": [
        "low — pure additive",
        "medium — needs migration plan",
        "low — gated behind opt-in flag",
        "medium — Postgres mirror still WIP",
        "low — schema is forward-compatible",
    ],
}


def fill_template(rng: random.Random, template: str) -> str:
    """Substitute every `{key}` in `template` with a random pick from VOCAB[key]."""
    out = template
    while "{" in out and "}" in out:
        start = out.find("{")
        end = out.find("}", start + 1)
        if end == -1:
            break
        key = out[start + 1 : end]
        choices = VOCAB.get(key)
        if not choices:
            # Unknown placeholder — leave as-is, will trip a unit test.
            break
        pick = rng.choice(choices)
        out = out[:start] + pick + out[end + 1 :]
    return out


def main() -> None:
    rng = random.Random(SEED)
    memories = []
    for idx in range(COUNT):
        ns = rng.choice(NAMESPACES)
        template = rng.choice(TEMPLATES)
        content = fill_template(rng, template)
        # Pad to make sure we always clear curator MIN_CONTENT_LEN (50). The
        # template baseline is well above 50 in practice; this guards against
        # a future tweak that shortens a vocab entry.
        if len(content) < 60:
            content = content + " " + "details pending follow-up sync."
        title = f"{ns.split('/')[-1]} #{idx:04d}"
        tag_count = rng.randint(0, 3)
        tags = rng.sample(TAG_POOL, k=tag_count) if tag_count else []
        tier = "long" if rng.random() < 0.4 else "mid"
        priority = rng.randint(3, 8)
        confidence = round(rng.uniform(0.6, 1.0), 2)
        memories.append(
            {
                "tier": tier,
                "namespace": ns,
                "title": title,
                "content": content,
                "tags": tags,
                "priority": priority,
                "confidence": confidence,
                "source": "import",
                "metadata": {},
            }
        )

    payload = {
        "schema_version": SCHEMA_VERSION,
        "description": (
            "v0.6.3 canonical workload — 1000-memory deterministic seed for "
            "the curator-cycle bench. Every entry is curator-eligible "
            "(public namespace, content >= 50 chars, no auto_tags) so a "
            "single sweep exercises auto_tag + detect_contradiction up to "
            "max_ops_per_cycle. Schema mirrors crate::models::CreateMemory "
            "for direct serde_json::from_str into the bench harness."
        ),
        "seed": SEED,
        "count": COUNT,
        "memories": memories,
    }

    out_path = Path(__file__).resolve().parent / "canonical_workload.json"
    out_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {out_path} ({len(memories)} memories)")


if __name__ == "__main__":
    main()
