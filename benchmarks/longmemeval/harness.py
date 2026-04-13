#!/usr/bin/env python3
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""LongMemEval benchmark harness for ai-memory.

Evaluates ai-memory's recall engine against the LongMemEval dataset,
computing Recall@K scores overall and per question category.

Usage:
    # Clone the dataset first:
    #   git clone https://github.com/xiaowu0162/LongMemEval /tmp/LongMemEval

    # Run on keyword tier:
    python harness.py --dataset-path /tmp/LongMemEval --variant S --tier keyword

    # Compare all tiers:
    python harness.py --dataset-path /tmp/LongMemEval --variant S --all-tiers

    # Custom K values:
    python harness.py --dataset-path /tmp/LongMemEval --variant S --tier semantic -k 5 -k 10 -k 20
"""

import argparse
import csv
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from pathlib import Path

try:
    from tabulate import tabulate
except ImportError:
    tabulate = None
    def _simple_table(data, headers):
        widths = [max(len(str(h)), max((len(str(r[i])) for r in data), default=0))
                  for i, h in enumerate(headers)]
        fmt = "  ".join(f"{{:<{w}}}" for w in widths)
        lines = [fmt.format(*headers), fmt.format(*("-" * w for w in widths))]
        for row in data:
            lines.append(fmt.format(*row))
        return "\n".join(lines)


def find_binary():
    """Locate the ai-memory binary."""
    # Check PATH first
    which = shutil.which("ai-memory")
    if which:
        return which
    # Check common build locations
    for candidate in [
        Path(__file__).resolve().parents[2] / "target" / "release" / "ai-memory",
        Path(__file__).resolve().parents[2] / "target" / "debug" / "ai-memory",
    ]:
        if candidate.exists():
            return str(candidate)
    print("Error: ai-memory binary not found. Build with 'cargo build --release' first.", file=sys.stderr)
    sys.exit(1)


def load_dataset(dataset_path, variant):
    """Load a LongMemEval JSON file."""
    variant_lower = variant.lower()
    candidates = [
        f"longmemeval_{variant_lower}_cleaned.json",
        f"longmemeval_{variant_lower}.json",
        f"data/longmemeval_{variant_lower}_cleaned.json",
        f"data/longmemeval_{variant_lower}.json",
    ]
    for name in candidates:
        path = Path(dataset_path) / name
        if path.exists():
            print(f"Loading dataset: {path}")
            with open(path) as f:
                data = json.load(f)
            print(f"Loaded {len(data)} evaluation instances")
            return data
    print(f"Error: Dataset file not found. Tried: {', '.join(candidates)}", file=sys.stderr)
    print(f"In directory: {dataset_path}", file=sys.stderr)
    sys.exit(1)


def ingest_sessions(binary, db_path, instance, per_turn=False):
    """Ingest all haystack sessions from one evaluation instance as memories.

    If per_turn is True, each user/assistant exchange is stored as a separate
    memory (more realistic — matches real ai-memory usage).  The session ID
    tag is preserved on every memory so recall scoring still works.
    """
    sessions = instance["haystack_sessions"]
    session_ids = instance["haystack_session_ids"]

    for i, (session, sid) in enumerate(zip(sessions, session_ids)):
        if per_turn:
            # Store each meaningful exchange as its own memory
            turn_idx = 0
            for turn in session:
                role = turn.get("role", "unknown")
                content = turn.get("content", "").strip()
                if not content:
                    continue
                # Title: first 100 chars of content
                title = content[:100]
                # Truncate content to 64KB
                if len(content) > 65000:
                    content = content[:65000]
                cmd = [
                    binary, "--db", db_path, "store",
                    "--tier", "long",
                    "--namespace", "longmemeval",
                    "--title", title,
                    "--content", f"[{role}]: {content}",
                    "--source", "import",
                    "--tags", f"sid:{sid}",
                    "--priority", "5",
                    "--json",
                ]
                result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
                if result.returncode != 0 and turn_idx == 0:
                    print(f"  Warning: Failed to store turn from {sid}: {result.stderr.strip()}", file=sys.stderr)
                turn_idx += 1
        else:
            # Original: store entire session as one memory
            lines = []
            for turn in session:
                role = turn.get("role", "unknown")
                content = turn.get("content", "")
                lines.append(f"[{role}]: {content}")
            full_content = "\n".join(lines)

            # Truncate to 64KB (ai-memory limit)
            if len(full_content) > 65000:
                full_content = full_content[:65000]

            # Title from first user message or session ID
            title = f"Session {sid}"
            for turn in session:
                if turn.get("role") == "user" and turn.get("content", "").strip():
                    title = turn["content"].strip()[:100]
                    break

            cmd = [
                binary, "--db", db_path, "store",
                "--tier", "long",
                "--namespace", "longmemeval",
                "--title", title,
                "--content", full_content,
                "--source", "import",
                "--tags", f"sid:{sid}",
                "--priority", "5",
                "--json",
            ]

            result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            if result.returncode != 0:
                print(f"  Warning: Failed to store session {sid}: {result.stderr.strip()}", file=sys.stderr)


def evaluate_question(binary, db_path, question_text, ground_truth_sids, k_values, tier):
    """Run recall for a question and check if ground truth sessions appear in top-K."""
    max_k = max(k_values)

    cmd = [
        binary, "--db", db_path, "--json",
        "recall", question_text,
        "--limit", str(max_k),
        "--namespace", "longmemeval",
        "--tier", tier,
    ]

    # Semantic/smart/autonomous tiers need more time for embedding backfill
    timeout = 30 if tier == "keyword" else 300
    result = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    if result.returncode != 0:
        return {k: False for k in k_values}, []

    try:
        output = json.loads(result.stdout)
    except json.JSONDecodeError:
        return {k: False for k in k_values}, []

    # Extract recalled memories
    memories = output.get("memories", output.get("results", []))
    if isinstance(output, list):
        memories = output

    # Extract session IDs from tags of recalled memories
    recalled_sids = []
    for mem in memories:
        tags = mem.get("tags", "")
        if isinstance(tags, str):
            tag_list = [t.strip() for t in tags.split(",")]
        elif isinstance(tags, list):
            tag_list = tags
        else:
            tag_list = []
        for tag in tag_list:
            if tag.startswith("sid:"):
                recalled_sids.append(tag[4:])
                break

    # Check recall at each K
    gt_set = set(str(s) for s in ground_truth_sids)
    hits = {}
    for k in k_values:
        top_k_sids = set(recalled_sids[:k])
        hits[k] = bool(gt_set & top_k_sids)

    return hits, recalled_sids


def run_benchmark(binary, dataset, tier, k_values, verbose=False, per_turn=False):
    """Run full benchmark for one tier. Returns per-category and overall results."""
    results_by_category = defaultdict(lambda: {k: {"hits": 0, "total": 0} for k in k_values})
    overall = {k: {"hits": 0, "total": 0} for k in k_values}

    total = len(dataset)
    start_time = time.time()

    for idx, instance in enumerate(dataset):
        qid = instance["question_id"]
        qtype = instance["question_type"]
        question = instance["question"]
        gt_sids = instance["answer_session_ids"]

        # Normalize category (strip _abs suffix for grouping, track abstention separately)
        is_abstention = qtype.endswith("_abs")
        category = qtype.replace("_abs", "")
        if is_abstention:
            category = "abstention"

        # Create a fresh DB for each instance (each has its own haystack)
        with tempfile.TemporaryDirectory() as tmpdir:
            db_path = os.path.join(tmpdir, "bench.db")

            # Ingest all sessions for this instance
            ingest_sessions(binary, db_path, instance, per_turn=per_turn)

            # Evaluate the question
            hits, recalled = evaluate_question(binary, db_path, question, gt_sids, k_values, tier)

        # Record results
        for k in k_values:
            if hits[k]:
                results_by_category[category][k]["hits"] += 1
                overall[k]["hits"] += 1
            results_by_category[category][k]["total"] += 1
            overall[k]["total"] += 1

        if verbose or (idx + 1) % 50 == 0:
            elapsed = time.time() - start_time
            rate = (idx + 1) / elapsed if elapsed > 0 else 0
            r5 = overall[k_values[0]]["hits"] / overall[k_values[0]]["total"] * 100
            print(f"  [{idx+1}/{total}] R@{k_values[0]}: {r5:.1f}%  ({rate:.1f} q/s)")

    elapsed = time.time() - start_time
    return results_by_category, overall, elapsed


def format_results(tier, results_by_category, overall, k_values, elapsed):
    """Format results as a table."""
    rows = []
    for category in sorted(results_by_category.keys()):
        row = [category]
        for k in k_values:
            data = results_by_category[category][k]
            pct = data["hits"] / data["total"] * 100 if data["total"] > 0 else 0
            row.append(f"{pct:.1f}% ({data['hits']}/{data['total']})")
        rows.append(row)

    # Overall row
    row = ["OVERALL"]
    for k in k_values:
        data = overall[k]
        pct = data["hits"] / data["total"] * 100 if data["total"] > 0 else 0
        row.append(f"{pct:.1f}% ({data['hits']}/{data['total']})")
    rows.append(row)

    headers = ["Category"] + [f"R@{k}" for k in k_values]

    print(f"\n{'='*60}")
    print(f"  ai-memory LongMemEval Results — tier: {tier}")
    print(f"  Time: {elapsed:.1f}s ({len(rows)-1} categories, {overall[k_values[0]]['total']} questions)")
    print(f"{'='*60}\n")

    if tabulate:
        print(tabulate(rows, headers=headers, tablefmt="github"))
    else:
        print(_simple_table(rows, headers))

    print()
    return rows, headers


def save_csv(output_path, tier, rows, headers):
    """Save results to CSV."""
    path = Path(output_path) / f"results_{tier}.csv"
    with open(path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(headers)
        writer.writerows(rows)
    print(f"Results saved to {path}")


def main():
    parser = argparse.ArgumentParser(description="LongMemEval benchmark harness for ai-memory")
    parser.add_argument("--dataset-path", required=True, help="Path to cloned LongMemEval repo")
    parser.add_argument("--variant", default="S", choices=["S", "M", "s", "m"],
                        help="Dataset variant: S (~40 sessions) or M (~500 sessions)")
    parser.add_argument("--tier", default="keyword",
                        choices=["keyword", "semantic", "smart", "autonomous"],
                        help="ai-memory feature tier")
    parser.add_argument("--all-tiers", action="store_true",
                        help="Run all tiers and produce comparison")
    parser.add_argument("-k", type=int, action="append", dest="k_values",
                        help="K values for Recall@K (default: 5, 10, 20)")
    parser.add_argument("--output", default=None,
                        help="Output directory for CSV results (default: benchmarks/longmemeval/results/)")
    parser.add_argument("--verbose", action="store_true", help="Print per-question progress")
    parser.add_argument("--per-turn", action="store_true",
                        help="Store each conversation turn as a separate memory (more realistic)")
    parser.add_argument("--binary", default=None, help="Path to ai-memory binary")
    args = parser.parse_args()

    k_values = args.k_values or [5, 10, 20]
    k_values.sort()

    binary = args.binary or find_binary()
    print(f"Using binary: {binary}")

    # Verify binary works
    result = subprocess.run([binary, "stats", "--json"], capture_output=True, text=True)
    if result.returncode != 0 and "no such subcommand" not in result.stderr:
        # stats may fail on missing db, that's ok
        pass

    dataset = load_dataset(args.dataset_path, args.variant)

    output_dir = args.output or str(Path(__file__).parent / "results")
    os.makedirs(output_dir, exist_ok=True)

    tiers = ["keyword", "semantic", "smart", "autonomous"] if args.all_tiers else [args.tier]

    comparison = {}

    for tier in tiers:
        print(f"\n{'#'*60}")
        print(f"  Running benchmark: tier={tier}, variant={args.variant.upper()}")
        print(f"{'#'*60}\n")

        results_by_category, overall, elapsed = run_benchmark(
            binary, dataset, tier, k_values, verbose=args.verbose,
            per_turn=args.per_turn
        )
        rows, headers = format_results(tier, results_by_category, overall, k_values, elapsed)
        save_csv(output_dir, tier, rows, headers)

        # Store for comparison
        comparison[tier] = {
            k: overall[k]["hits"] / overall[k]["total"] * 100 if overall[k]["total"] > 0 else 0
            for k in k_values
        }

    # Print comparison table if multiple tiers
    if len(tiers) > 1:
        print(f"\n{'='*60}")
        print("  Tier Comparison")
        print(f"{'='*60}\n")
        comp_headers = ["Tier"] + [f"R@{k}" for k in k_values]
        comp_rows = [[tier] + [f"{comparison[tier][k]:.1f}%" for k in k_values] for tier in tiers]
        if tabulate:
            print(tabulate(comp_rows, headers=comp_headers, tablefmt="github"))
        else:
            print(_simple_table(comp_rows, comp_headers))
        print()

        # Save comparison CSV
        comp_path = Path(output_dir) / "comparison.csv"
        with open(comp_path, "w", newline="") as f:
            writer = csv.writer(f)
            writer.writerow(comp_headers)
            writer.writerows(comp_rows)
        print(f"Comparison saved to {comp_path}")


if __name__ == "__main__":
    main()
