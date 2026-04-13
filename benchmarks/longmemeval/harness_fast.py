#!/usr/bin/env python3
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Blazing fast LongMemEval benchmark — native Python + SQLite, zero subprocesses.

Replicates ai-memory's FTS5 scoring formula directly in Python/SQLite,
eliminating ~20,500 subprocess spawns per benchmark run.

For semantic/hybrid benchmarks, optionally calls Ollama embed API directly.

Usage:
    # Keyword (fastest — pure SQLite):
    python harness_fast.py --dataset-path /tmp/LongMemEval --variant S

    # With hybrid scoring via Ollama:
    python harness_fast.py --dataset-path /tmp/LongMemEval --variant S --hybrid

    # Compare keyword vs hybrid:
    python harness_fast.py --dataset-path /tmp/LongMemEval --variant S --compare
"""

import argparse
import json
import os
import re
import sqlite3
import sys
import time
import uuid
from collections import defaultdict
from pathlib import Path

try:
    from tabulate import tabulate
except ImportError:
    tabulate = None

try:
    import requests
    HAS_REQUESTS = True
except ImportError:
    HAS_REQUESTS = False


# ---------------------------------------------------------------------------
# Schema — exact replica of ai-memory's SQLite schema
# ---------------------------------------------------------------------------

SCHEMA = """
CREATE TABLE IF NOT EXISTS memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',
    priority         INTEGER NOT NULL DEFAULT 5,
    confidence       REAL NOT NULL DEFAULT 1.0,
    source           TEXT NOT NULL DEFAULT 'api',
    access_count     INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    last_accessed_at TEXT,
    expires_at       TEXT,
    embedding        BLOB
);

CREATE INDEX IF NOT EXISTS idx_memories_tier ON memories(tier);
CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
CREATE INDEX IF NOT EXISTS idx_memories_priority ON memories(priority DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_title_ns ON memories(title, namespace);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    title,
    content,
    tags,
    content=memories,
    content_rowid=rowid
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;
"""

# ---------------------------------------------------------------------------
# FTS5 query sanitization — mirrors ai-memory's sanitize_fts_query()
# ---------------------------------------------------------------------------

FTS_SPECIAL = set('"*^{}():|-')
FTS_OPERATORS = {"AND", "OR", "NOT", "NEAR"}


def sanitize_fts_query(text, use_or=True):
    joiner = " OR " if use_or else " "
    tokens = []
    for word in text.split():
        if word.upper() in FTS_OPERATORS:
            continue
        clean = "".join(c for c in word if c not in FTS_SPECIAL)
        if clean:
            tokens.append(f'"{clean}"')
    return joiner.join(tokens) if tokens else '"_empty_"'


# ---------------------------------------------------------------------------
# Scoring SQL — exact replica of ai-memory's recall scoring
# ---------------------------------------------------------------------------

RECALL_SQL = """
SELECT m.id, m.tags,
       (fts.rank * -1)
       + (m.priority * 0.5)
       + (MIN(m.access_count, 50) * 0.1)
       + (m.confidence * 2.0)
       + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
       + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
       AS score
FROM memories_fts fts
JOIN memories m ON m.rowid = fts.rowid
WHERE memories_fts MATCH ?1
  AND m.namespace = ?2
  AND (m.expires_at IS NULL OR m.expires_at > datetime('now'))
ORDER BY score DESC
LIMIT ?3
"""

# ---------------------------------------------------------------------------
# Ollama embedding (optional, for hybrid mode)
# ---------------------------------------------------------------------------

OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
EMBED_MODEL = "nomic-embed-text"
_embed_cache = {}


def ollama_embed(text, url=OLLAMA_URL):
    """Get embedding from Ollama. Caches by text hash."""
    key = hash(text[:2000])  # cache key on first 2K chars
    if key in _embed_cache:
        return _embed_cache[key]
    try:
        resp = requests.post(
            f"{url}/api/embed",
            json={"model": EMBED_MODEL, "input": text[:8000]},
            timeout=30,
        )
        resp.raise_for_status()
        emb = resp.json()["embeddings"][0]
        _embed_cache[key] = emb
        return emb
    except Exception:
        return None


def cosine_similarity(a, b):
    if len(a) != len(b):
        return 0.0
    dot = sum(x * y for x, y in zip(a, b))
    na = sum(x * x for x in a) ** 0.5
    nb = sum(x * x for x in b) ** 0.5
    if na == 0 or nb == 0:
        return 0.0
    return dot / (na * nb)


# ---------------------------------------------------------------------------
# Dataset loading
# ---------------------------------------------------------------------------

def load_dataset(dataset_path, variant):
    variant_lower = variant.lower()
    for name in [
        f"longmemeval_{variant_lower}_cleaned.json",
        f"data/longmemeval_{variant_lower}_cleaned.json",
        f"longmemeval_{variant_lower}.json",
        f"data/longmemeval_{variant_lower}.json",
    ]:
        path = Path(dataset_path) / name
        if path.exists():
            print(f"Loading dataset: {path}")
            with open(path) as f:
                data = json.load(f)
            print(f"Loaded {len(data)} evaluation instances")
            return data
    print(f"Error: Dataset not found in {dataset_path}", file=sys.stderr)
    sys.exit(1)


# ---------------------------------------------------------------------------
# Core benchmark logic — all in-process, zero subprocesses
# ---------------------------------------------------------------------------

def run_instance(instance, k_values, namespace, hybrid=False):
    """Run one evaluation instance entirely in-memory. Returns hit dict."""
    sessions = instance["haystack_sessions"]
    session_ids = instance["haystack_session_ids"]
    gt_sids = set(str(s) for s in instance["answer_session_ids"])
    question = instance["question"]

    # Create in-memory SQLite DB
    conn = sqlite3.connect(":memory:")
    conn.execute("PRAGMA journal_mode=OFF")
    conn.execute("PRAGMA synchronous=OFF")
    conn.executescript(SCHEMA)

    now = time.strftime("%Y-%m-%dT%H:%M:%S+00:00")

    # Ingest all sessions
    for session, sid in zip(sessions, session_ids):
        lines = []
        for turn in session:
            role = turn.get("role", "unknown")
            content = turn.get("content", "")
            lines.append(f"[{role}]: {content}")
        full_content = "\n".join(lines)
        if len(full_content) > 65000:
            full_content = full_content[:65000]

        title = f"Session {sid}"
        for turn in session:
            if turn.get("role") == "user" and turn.get("content", "").strip():
                title = turn["content"].strip()[:100]
                break

        tags_json = json.dumps([f"sid:{sid}"])
        mem_id = str(uuid.uuid4())

        try:
            conn.execute(
                """INSERT OR REPLACE INTO memories
                   (id, tier, namespace, title, content, tags, priority,
                    confidence, source, access_count, created_at, updated_at)
                   VALUES (?, 'long', ?, ?, ?, ?, 5, 1.0, 'import', 0, ?, ?)""",
                (mem_id, namespace, title, full_content, tags_json, now, now),
            )
        except sqlite3.IntegrityError:
            pass

    conn.commit()

    # Recall
    max_k = max(k_values)
    fts_query = sanitize_fts_query(question)

    if hybrid and HAS_REQUESTS:
        # Hybrid mode: FTS + semantic scoring with adaptive blend
        rows = conn.execute(
            """SELECT m.id, m.tags, m.content,
                      (fts.rank * -1)
                      + (m.priority * 0.5)
                      + (MIN(m.access_count, 50) * 0.1)
                      + (m.confidence * 2.0)
                      + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
                      + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
                      AS fts_score
               FROM memories_fts fts
               JOIN memories m ON m.rowid = fts.rowid
               WHERE memories_fts MATCH ?1
                 AND m.namespace = ?2
               ORDER BY fts_score DESC
               LIMIT ?3""",
            (fts_query, namespace, max_k * 3),
        ).fetchall()

        if rows:
            query_emb = ollama_embed(question)
            if query_emb:
                max_fts = max(r[3] for r in rows) or 1.0
                scored = []
                for row_id, row_tags, row_content, fts_score in rows:
                    content_emb = ollama_embed(row_content[:4000])
                    cosine = cosine_similarity(query_emb, content_emb) if content_emb else 0.0
                    norm_fts = fts_score / max_fts
                    content_len = len(row_content)
                    # Adaptive blend
                    if content_len <= 500:
                        sw = 0.50
                    elif content_len >= 5000:
                        sw = 0.15
                    else:
                        sw = 0.50 - 0.35 * ((content_len - 500) / 4500)
                    blended = sw * cosine + (1.0 - sw) * norm_fts
                    scored.append((row_id, row_tags, blended))
                scored.sort(key=lambda x: -x[2])
                rows = [(r[0], r[1]) for r in scored[:max_k]]
            else:
                rows = [(r[0], r[1]) for r in rows[:max_k]]
        else:
            rows = []
    else:
        # Pure keyword mode
        rows = conn.execute(RECALL_SQL, (fts_query, namespace, max_k)).fetchall()

    # Extract session IDs from recalled memories
    recalled_sids = []
    for row in rows:
        tags = row[1]
        try:
            tag_list = json.loads(tags)
        except (json.JSONDecodeError, TypeError):
            tag_list = []
        for tag in tag_list:
            if tag.startswith("sid:"):
                recalled_sids.append(tag[4:])
                break

    conn.close()

    # Check recall at each K
    hits = {}
    for k in k_values:
        top_k = set(recalled_sids[:k])
        hits[k] = bool(gt_sids & top_k)
    return hits


def run_benchmark(dataset, k_values, hybrid=False):
    """Run full benchmark. Returns per-category and overall results."""
    results_by_category = defaultdict(lambda: {k: {"hits": 0, "total": 0} for k in k_values})
    overall = {k: {"hits": 0, "total": 0} for k in k_values}
    namespace = "longmemeval"
    total = len(dataset)
    start_time = time.time()

    for idx, instance in enumerate(dataset):
        qtype = instance["question_type"]
        is_abstention = qtype.endswith("_abs")
        category = qtype.replace("_abs", "")
        if is_abstention:
            category = "abstention"

        hits = run_instance(instance, k_values, namespace, hybrid=hybrid)

        for k in k_values:
            if hits[k]:
                results_by_category[category][k]["hits"] += 1
                overall[k]["hits"] += 1
            results_by_category[category][k]["total"] += 1
            overall[k]["total"] += 1

        if (idx + 1) % 50 == 0:
            elapsed = time.time() - start_time
            rate = (idx + 1) / elapsed
            r1 = overall[k_values[0]]["hits"] / overall[k_values[0]]["total"] * 100
            print(f"  [{idx+1}/{total}] R@{k_values[0]}: {r1:.1f}%  ({rate:.1f} q/s)")

    elapsed = time.time() - start_time
    return results_by_category, overall, elapsed


def format_results(label, results_by_category, overall, k_values, elapsed):
    rows = []
    for category in sorted(results_by_category.keys()):
        row = [category]
        for k in k_values:
            data = results_by_category[category][k]
            pct = data["hits"] / data["total"] * 100 if data["total"] > 0 else 0
            row.append(f"{pct:.1f}% ({data['hits']}/{data['total']})")
        rows.append(row)

    row = ["OVERALL"]
    for k in k_values:
        data = overall[k]
        pct = data["hits"] / data["total"] * 100 if data["total"] > 0 else 0
        row.append(f"{pct:.1f}% ({data['hits']}/{data['total']})")
    rows.append(row)

    headers = ["Category"] + [f"R@{k}" for k in k_values]

    print(f"\n{'='*60}")
    print(f"  ai-memory LongMemEval Results — {label}")
    print(f"  Time: {elapsed:.1f}s ({len(rows)-1} categories, {overall[k_values[0]]['total']} questions)")
    print(f"{'='*60}\n")

    if tabulate:
        print(tabulate(rows, headers=headers, tablefmt="github"))
    else:
        widths = [max(len(str(h)), max((len(str(r[i])) for r in rows), default=0))
                  for i, h in enumerate(headers)]
        fmt = "  ".join(f"{{:<{w}}}" for w in widths)
        print(fmt.format(*headers))
        print(fmt.format(*("-" * w for w in widths)))
        for row in rows:
            print(fmt.format(*row))
    print()
    return rows, headers


def main():
    parser = argparse.ArgumentParser(
        description="Blazing fast LongMemEval benchmark — native Python + SQLite"
    )
    parser.add_argument("--dataset-path", required=True)
    parser.add_argument("--variant", default="S", choices=["S", "M", "s", "m"])
    parser.add_argument("-k", type=int, action="append", dest="k_values")
    parser.add_argument("--hybrid", action="store_true",
                        help="Enable hybrid scoring (requires Ollama)")
    parser.add_argument("--compare", action="store_true",
                        help="Run both keyword and hybrid, print comparison")
    args = parser.parse_args()

    k_values = sorted(args.k_values or [1, 5, 10, 20])
    dataset = load_dataset(args.dataset_path, args.variant)

    modes = []
    if args.compare:
        modes = [("keyword (FTS5)", False), ("hybrid (adaptive)", True)]
    else:
        modes = [("hybrid (adaptive)" if args.hybrid else "keyword (FTS5)", args.hybrid)]

    comparison = {}
    for label, hybrid in modes:
        print(f"\n{'#'*60}")
        print(f"  Running: {label}")
        print(f"{'#'*60}\n")

        by_cat, overall, elapsed = run_benchmark(dataset, k_values, hybrid=hybrid)
        format_results(label, by_cat, overall, k_values, elapsed)
        comparison[label] = {
            k: overall[k]["hits"] / overall[k]["total"] * 100
            for k in k_values
        }

    if len(modes) > 1:
        print(f"\n{'='*60}")
        print("  Comparison")
        print(f"{'='*60}\n")
        comp_headers = ["Mode"] + [f"R@{k}" for k in k_values]
        comp_rows = [
            [label] + [f"{comparison[label][k]:.1f}%" for k in k_values]
            for label, _ in modes
        ]
        if tabulate:
            print(tabulate(comp_rows, headers=comp_headers, tablefmt="github"))
        else:
            widths = [max(len(str(h)), max((len(str(r[i])) for r in comp_rows), default=0))
                      for i, h in enumerate(comp_headers)]
            fmt = "  ".join(f"{{:<{w}}}" for w in widths)
            print(fmt.format(*comp_headers))
            print(fmt.format(*("-" * w for w in widths)))
            for row in comp_rows:
                print(fmt.format(*row))
        print()


if __name__ == "__main__":
    main()
