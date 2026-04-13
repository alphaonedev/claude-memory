#!/usr/bin/env python3
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Blazing LongMemEval benchmark — multi-strategy FTS5 recall for maximum accuracy.

Runs multiple FTS query strategies per question and merges results.
At 55+ q/s base rate, 3 strategies still finishes in ~30 seconds.

Target: 99%+ R@5 on LongMemEval-S.
"""

import argparse
import json
import os
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

# ---------------------------------------------------------------------------
# Schema
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
CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_title_ns ON memories(title, namespace);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    title,
    content,
    tags,
    content=memories,
    content_rowid=rowid,
    columnsize=0
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;
"""

# ---------------------------------------------------------------------------
# Stop words — stripped for tighter FTS matching
# ---------------------------------------------------------------------------

STOP_WORDS = {
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "could",
    "should", "may", "might", "shall", "can", "need", "dare", "ought",
    "i", "me", "my", "mine", "we", "us", "our", "ours", "you", "your",
    "yours", "he", "him", "his", "she", "her", "hers", "it", "its",
    "they", "them", "their", "theirs", "what", "which", "who", "whom",
    "this", "that", "these", "those", "am", "to", "of", "in", "for",
    "on", "with", "at", "by", "from", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "out", "off", "over",
    "under", "again", "further", "then", "once", "here", "there", "when",
    "where", "why", "how", "all", "both", "each", "few", "more", "most",
    "other", "some", "such", "no", "nor", "not", "only", "own", "same",
    "so", "than", "too", "very", "just", "about", "also", "and", "but",
    "if", "or", "because", "until", "while", "up", "down", "any",
    "tell", "told", "said", "say", "know", "think", "like", "get",
    "go", "going", "went", "come", "came", "make", "made", "take",
    "took", "give", "gave", "find", "found", "see", "saw", "want",
    "wanted", "look", "looked", "use", "used", "work", "worked",
}

FTS_SPECIAL = set('"*^{}():|-')
FTS_OPERATORS = {"AND", "OR", "NOT", "NEAR"}


def clean_token(word):
    """Strip FTS5 special chars from a token."""
    clean = "".join(c for c in word if c not in FTS_SPECIAL)
    return clean if clean and clean.upper() not in FTS_OPERATORS else ""


def sanitize_fts_or(text):
    """Standard OR query — matches any term."""
    tokens = [f'"{clean_token(w)}"' for w in text.split() if clean_token(w)]
    return " OR ".join(tokens) if tokens else '"_empty_"'


def sanitize_fts_content_words(text):
    """Stop-word-stripped OR query — focuses on content words."""
    tokens = []
    for w in text.split():
        c = clean_token(w)
        if c and c.lower() not in STOP_WORDS:
            tokens.append(f'"{c}"')
    return " OR ".join(tokens) if tokens else None


def sanitize_fts_prefix(text):
    """Prefix query — matches partial words with wildcards."""
    tokens = []
    for w in text.split():
        c = clean_token(w)
        if c and c.lower() not in STOP_WORDS and len(c) >= 3:
            tokens.append(f'"{c}"*')
    return " OR ".join(tokens) if tokens else None


def sanitize_fts_phrases(text):
    """Bigram phrase query — matches adjacent word pairs."""
    words = [clean_token(w) for w in text.split() if clean_token(w)]
    words = [w for w in words if w.lower() not in STOP_WORDS]
    if len(words) < 2:
        return None
    phrases = []
    for i in range(len(words) - 1):
        phrases.append(f'"{words[i]} {words[i+1]}"')
    return " OR ".join(phrases) if phrases else None


# ---------------------------------------------------------------------------
# Scoring SQL
# ---------------------------------------------------------------------------

RECALL_SQL = """
SELECT m.id, m.tags,
       (bm25(memories_fts, 5.0, 1.0, 0.5) * -1)
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
ORDER BY score DESC
LIMIT ?3
"""


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
# Multi-strategy recall
# ---------------------------------------------------------------------------

def extract_entity_query(text):
    """Strip question framing words and extract entity-focused terms.

    Turns "How many times did I bake something?" into '"bake" OR "something"'
    Turns "What kitchen appliance did I buy 10 days ago?" into '"kitchen" OR "appliance" OR "buy"'
    """
    # Additional question-framing words to strip beyond stop words
    q_frames = {
        "how", "many", "much", "what", "when", "where", "which", "who",
        "whom", "whose", "why", "does", "do", "did", "is", "are", "was",
        "were", "can", "could", "would", "should", "will", "shall",
        "total", "number", "different", "times", "time", "ago", "past",
        "during", "weeks", "week", "days", "day", "months", "month",
        "years", "year", "recently", "lately", "last", "first",
        "mentioned", "mentioned", "remember", "recall",
        "ever", "always", "usually", "often", "sometimes",
    }
    all_stop = STOP_WORDS | q_frames
    tokens = []
    for w in text.split():
        c = clean_token(w)
        if c and c.lower() not in all_stop and len(c) >= 3:
            tokens.append(f'"{c}"')
    return " OR ".join(tokens) if tokens else None


def multi_recall(conn, question, namespace, limit):
    """Primary FTS5 recall + entity-focused fallback to catch hard queries.

    Returns primary results first (preserving ranking), then appends
    any new results from entity query that weren't in the primary set.
    """
    # Primary: standard OR query
    primary_query = sanitize_fts_or(question)
    seen = set()
    results = []

    try:
        rows = conn.execute(RECALL_SQL, (primary_query, namespace, limit)).fetchall()
        for row_id, row_tags, _score in rows:
            seen.add(row_id)
            results.append((row_id, row_tags))
    except sqlite3.OperationalError:
        pass

    # If primary didn't fill the limit, try entity-focused query
    if len(results) < limit:
        for fallback_fn in [extract_entity_query, sanitize_fts_content_words,
                            sanitize_fts_prefix, sanitize_fts_phrases]:
            if len(results) >= limit:
                break
            fb_query = fallback_fn(question)
            if not fb_query:
                continue
            try:
                fb_rows = conn.execute(RECALL_SQL, (fb_query, namespace, limit)).fetchall()
                for row_id, row_tags, _score in fb_rows:
                    if row_id not in seen:
                        seen.add(row_id)
                        results.append((row_id, row_tags))
                        if len(results) >= limit:
                            break
            except sqlite3.OperationalError:
                continue

    return results[:limit]


def run_instance(instance, k_values, namespace):
    sessions = instance["haystack_sessions"]
    session_ids = instance["haystack_session_ids"]
    gt_sids = set(str(s) for s in instance["answer_session_ids"])
    question = instance["question"]

    conn = sqlite3.connect(":memory:")
    conn.execute("PRAGMA journal_mode=OFF")
    conn.execute("PRAGMA synchronous=OFF")
    conn.executescript(SCHEMA)

    now = time.strftime("%Y-%m-%dT%H:%M:%S+00:00")

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

        # Enhanced title: ALL user messages (5x BM25 weight on title column)
        title = " | ".join(user_msgs)[:500] if user_msgs else f"Session {sid}"
        tags_json = json.dumps([f"sid:{sid}"])

        try:
            conn.execute(
                """INSERT OR REPLACE INTO memories
                   (id, tier, namespace, title, content, tags, priority,
                    confidence, source, access_count, created_at, updated_at)
                   VALUES (?, 'long', ?, ?, ?, ?, 5, 1.0, 'import', 0, ?, ?)""",
                (str(uuid.uuid4()), namespace, title, full_content, tags_json, now, now),
            )
        except sqlite3.IntegrityError:
            pass
    conn.commit()

    max_k = max(k_values)
    # Fetch extra results so we can deduplicate by session ID
    rows = multi_recall(conn, question, namespace, max_k * 3)

    # Deduplicate by session ID — first occurrence wins (best score)
    recalled_sids = []
    seen_sids = set()
    for row_id, tags in rows:
        try:
            tag_list = json.loads(tags)
        except (json.JSONDecodeError, TypeError):
            tag_list = []
        for tag in tag_list:
            if tag.startswith("sid:"):
                sid = tag[4:]
                if sid not in seen_sids:
                    seen_sids.add(sid)
                    recalled_sids.append(sid)
                break

    conn.close()

    hits = {}
    for k in k_values:
        top_k = set(recalled_sids[:k])
        hits[k] = bool(gt_sids & top_k)
    return hits


def run_benchmark(dataset, k_values):
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

        hits = run_instance(instance, k_values, namespace)

        # Log R@5 misses for analysis
        if 5 in k_values and not hits.get(5, True):
            print(f"  MISS@5 [{category}] q={instance['question'][:80]}", file=sys.stderr)

        for k in k_values:
            if hits[k]:
                results_by_category[category][k]["hits"] += 1
                overall[k]["hits"] += 1
            results_by_category[category][k]["total"] += 1
            overall[k]["total"] += 1

        if (idx + 1) % 50 == 0:
            elapsed = time.time() - start_time
            rate = (idx + 1) / elapsed
            r5 = overall[5]["hits"] / overall[5]["total"] * 100 if overall[5]["total"] else 0
            r1 = overall[k_values[0]]["hits"] / overall[k_values[0]]["total"] * 100
            print(f"  [{idx+1}/{total}] R@1: {r1:.1f}%  R@5: {r5:.1f}%  ({rate:.1f} q/s)")

    elapsed = time.time() - start_time
    return results_by_category, overall, elapsed


def main():
    parser = argparse.ArgumentParser(description="Blazing LongMemEval — multi-strategy FTS5")
    parser.add_argument("--dataset-path", required=True)
    parser.add_argument("--variant", default="S", choices=["S", "M", "s", "m"])
    parser.add_argument("-k", type=int, action="append", dest="k_values")
    args = parser.parse_args()

    k_values = sorted(args.k_values or [1, 5, 10, 20])
    dataset = load_dataset(args.dataset_path, args.variant)

    print(f"\n{'#'*60}")
    print(f"  BLAZING MODE — multi-strategy FTS5 recall")
    print(f"{'#'*60}\n")

    by_cat, overall, elapsed = run_benchmark(dataset, k_values)

    rows = []
    for category in sorted(by_cat.keys()):
        row = [category]
        for k in k_values:
            data = by_cat[category][k]
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
    print(f"  ai-memory LongMemEval — BLAZING multi-strategy")
    print(f"  Time: {elapsed:.1f}s  ({overall[k_values[0]]['total']} questions)")
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
