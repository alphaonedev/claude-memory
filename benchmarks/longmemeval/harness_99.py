#!/usr/bin/env python3
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""99%+ R@5 LongMemEval — parallel LLM expansion + parallel FTS5 recall.

Phase 1: Expand all 500 queries via Ollama (threaded, 16 workers)
Phase 2: FTS5 recall across 10 CPU cores (multiprocessing)

Both phases fully parallelized. Target: 99%+ R@5, under 60s total.
"""

import argparse
import json
import multiprocessing as mp
import os
import sqlite3
import sys
import time
import uuid
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

try:
    from tabulate import tabulate
except ImportError:
    tabulate = None

import requests

OLLAMA_URL = os.environ.get("OLLAMA_URL", "http://localhost:11434")
LLM_MODEL = os.environ.get("LLM_MODEL", "gemma3:4b")

# ---------------------------------------------------------------------------
# Schema + FTS helpers
# ---------------------------------------------------------------------------

SCHEMA = """
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY, tier TEXT NOT NULL, namespace TEXT NOT NULL DEFAULT 'global',
    title TEXT NOT NULL, content TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '[]',
    priority INTEGER NOT NULL DEFAULT 5, confidence REAL NOT NULL DEFAULT 1.0,
    source TEXT NOT NULL DEFAULT 'api', access_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    last_accessed_at TEXT, expires_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_ns ON memories(namespace);
CREATE UNIQUE INDEX IF NOT EXISTS idx_title_ns ON memories(title, namespace);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    title, content, tags, content=memories, content_rowid=rowid
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;
"""

FTS_SPECIAL = set('"*^{}():|-')
FTS_OPERATORS = {"AND", "OR", "NOT", "NEAR"}

RECALL_SQL = """
SELECT m.id, m.tags,
       (fts.rank * -1) + (m.priority * 0.5) + (MIN(m.access_count,50)*0.1)
       + (m.confidence * 2.0)
       + (CASE m.tier WHEN 'long' THEN 3.0 WHEN 'mid' THEN 1.0 ELSE 0.0 END)
       + (1.0 / (1.0 + (julianday('now') - julianday(m.updated_at)) * 0.1))
FROM memories_fts fts JOIN memories m ON m.rowid = fts.rowid
WHERE memories_fts MATCH ?1 AND m.namespace = ?2
ORDER BY 3 DESC LIMIT ?3
"""


def sanitize_fts_or(text):
    tokens = []
    for w in text.split():
        c = "".join(ch for ch in w if ch not in FTS_SPECIAL)
        if c and c.upper() not in FTS_OPERATORS:
            tokens.append(f'"{c}"')
    return " OR ".join(tokens) if tokens else '"_empty_"'


# ---------------------------------------------------------------------------
# LLM expansion
# ---------------------------------------------------------------------------

EXPAND_PROMPT = """Generate 8-15 search keywords/synonyms for this question. Output ONLY comma-separated keywords. Include activity words, synonyms, related terms.

Question: {question}

Keywords:"""


def expand_one(args):
    idx, question, url, model = args
    try:
        resp = requests.post(
            f"{url}/api/generate",
            json={"model": model, "prompt": EXPAND_PROMPT.format(question=question),
                  "stream": False, "options": {"temperature": 0.3, "num_predict": 80}},
            timeout=30)
        resp.raise_for_status()
        return idx, resp.json().get("response", "").strip().replace("\n", " ")
    except Exception:
        return idx, ""


# ---------------------------------------------------------------------------
# Single instance evaluation (runs in worker process)
# ---------------------------------------------------------------------------

def eval_instance(args):
    """Evaluate one instance. Designed for multiprocessing.Pool.map()."""
    instance, k_values, namespace, expansion = args

    sessions = instance["haystack_sessions"]
    session_ids = instance["haystack_session_ids"]
    gt_sids = set(str(s) for s in instance["answer_session_ids"])
    question = instance["question"]
    qtype = instance["question_type"]

    conn = sqlite3.connect(":memory:")
    conn.execute("PRAGMA journal_mode=OFF")
    conn.execute("PRAGMA synchronous=OFF")
    conn.executescript(SCHEMA)

    now = time.strftime("%Y-%m-%dT%H:%M:%S+00:00")

    # Ingest
    for session, sid in zip(sessions, session_ids):
        lines = []
        user_msgs = []
        for turn in session:
            role = turn.get("role", "unknown")
            content = turn.get("content", "")
            lines.append(f"[{role}]: {content}")
            if role == "user" and content.strip():
                user_msgs.append(content.strip())

        full_content = "\n".join(lines)[:65000]
        title = " | ".join(user_msgs)[:500] if user_msgs else f"Session {sid}"
        tags_json = json.dumps([f"sid:{sid}"])

        try:
            conn.execute(
                "INSERT OR REPLACE INTO memories (id,tier,namespace,title,content,tags,"
                "priority,confidence,source,access_count,created_at,updated_at) "
                "VALUES (?,'long',?,?,?,?,5,1.0,'import',0,?,?)",
                (str(uuid.uuid4()), namespace, title, full_content, tags_json, now, now))
        except sqlite3.IntegrityError:
            pass
    conn.commit()

    max_k = max(k_values)

    # Recall with expanded query
    combined = f"{question} {expansion}" if expansion else question
    fts_query = sanitize_fts_or(combined)

    seen_ids = set()
    results = []

    try:
        rows = conn.execute(RECALL_SQL, (fts_query, namespace, max_k * 3)).fetchall()
        for row_id, row_tags, _ in rows:
            if row_id not in seen_ids:
                seen_ids.add(row_id)
                results.append(row_tags)
    except sqlite3.OperationalError:
        pass

    # Fallback: original query only
    if len(results) < max_k:
        orig_query = sanitize_fts_or(question)
        try:
            rows = conn.execute(RECALL_SQL, (orig_query, namespace, max_k * 2)).fetchall()
            for row_id, row_tags, _ in rows:
                if row_id not in seen_ids:
                    seen_ids.add(row_id)
                    results.append(row_tags)
        except sqlite3.OperationalError:
            pass

    conn.close()

    # Extract session IDs
    recalled_sids = []
    seen_sids = set()
    for tags in results:
        try:
            tag_list = json.loads(tags)
        except (json.JSONDecodeError, TypeError):
            continue
        for tag in tag_list:
            if tag.startswith("sid:"):
                sid = tag[4:]
                if sid not in seen_sids:
                    seen_sids.add(sid)
                    recalled_sids.append(sid)
                break

    hits = {}
    for k in k_values:
        hits[k] = bool(gt_sids & set(recalled_sids[:k]))

    return qtype, hits


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def load_dataset(dataset_path, variant):
    for name in [f"longmemeval_{variant.lower()}_cleaned.json",
                 f"data/longmemeval_{variant.lower()}_cleaned.json"]:
        path = Path(dataset_path) / name
        if path.exists():
            with open(path) as f:
                data = json.load(f)
            print(f"Loaded {len(data)} instances from {path}")
            return data
    sys.exit(f"Dataset not found in {dataset_path}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset-path", required=True)
    parser.add_argument("--variant", default="S")
    parser.add_argument("-k", type=int, action="append", dest="k_values")
    parser.add_argument("--workers", type=int, default=min(10, os.cpu_count() or 4))
    parser.add_argument("--llm-workers", type=int, default=16)
    parser.add_argument("--model", default=None)
    parser.add_argument("--no-expand", action="store_true", help="Skip LLM expansion")
    args = parser.parse_args()

    global LLM_MODEL
    if args.model:
        LLM_MODEL = args.model

    k_values = sorted(args.k_values or [1, 5, 10, 20])
    dataset = load_dataset(args.dataset_path, args.variant)
    total = len(dataset)

    # Phase 1: LLM expansion (threaded I/O parallelism)
    expansions = [""] * total
    if not args.no_expand:
        print(f"\n--- Phase 1: LLM expansion ({LLM_MODEL}, {args.llm_workers} threads) ---")
        t0 = time.time()
        work = [(i, dataset[i]["question"], OLLAMA_URL, LLM_MODEL) for i in range(total)]
        with ThreadPoolExecutor(max_workers=args.llm_workers) as pool:
            futures = {pool.submit(expand_one, w): w[0] for w in work}
            done = 0
            for f in as_completed(futures):
                idx, exp = f.result()
                expansions[idx] = exp
                done += 1
                if done % 100 == 0:
                    print(f"  [{done}/{total}] expanded ({done/(time.time()-t0):.1f} q/s)")
        print(f"  Expansion: {time.time()-t0:.1f}s ({total/(time.time()-t0):.1f} q/s)")

    # Phase 2: Parallel FTS5 recall (CPU parallelism)
    print(f"\n--- Phase 2: FTS5 recall ({args.workers} processes) ---")
    namespace = "longmemeval"
    work = [(dataset[i], k_values, namespace, expansions[i]) for i in range(total)]

    t0 = time.time()
    with mp.Pool(args.workers) as pool:
        results = pool.map(eval_instance, work)
    recall_time = time.time() - t0
    print(f"  Recall: {recall_time:.1f}s ({total/recall_time:.1f} q/s)")

    # Aggregate
    by_cat = defaultdict(lambda: {k: {"hits": 0, "total": 0} for k in k_values})
    overall = {k: {"hits": 0, "total": 0} for k in k_values}

    for qtype, hits in results:
        is_abs = qtype.endswith("_abs")
        cat = qtype.replace("_abs", "")
        if is_abs:
            cat = "abstention"
        for k in k_values:
            if hits[k]:
                by_cat[cat][k]["hits"] += 1
                overall[k]["hits"] += 1
            by_cat[cat][k]["total"] += 1
            overall[k]["total"] += 1

    # Print
    rows = []
    for cat in sorted(by_cat.keys()):
        row = [cat]
        for k in k_values:
            d = by_cat[cat][k]
            pct = d["hits"] / d["total"] * 100 if d["total"] else 0
            row.append(f"{pct:.1f}% ({d['hits']}/{d['total']})")
        rows.append(row)
    row = ["OVERALL"]
    for k in k_values:
        d = overall[k]
        pct = d["hits"] / d["total"] * 100 if d["total"] else 0
        row.append(f"{pct:.1f}% ({d['hits']}/{d['total']})")
    rows.append(row)

    headers = ["Category"] + [f"R@{k}" for k in k_values]
    print(f"\n{'='*60}")
    label = f"LLM-expanded ({LLM_MODEL})" if not args.no_expand else "keyword (FTS5)"
    print(f"  ai-memory LongMemEval — {label}")
    print(f"  Recall: {recall_time:.1f}s  |  Workers: {args.workers}")
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


if __name__ == "__main__":
    main()
