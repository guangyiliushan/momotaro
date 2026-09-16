#!/usr/bin/env python3
"""Momotaro retrieval eval: runs `momotaro search --json` over the golden set.

Computes recall@k and MRR. Pure standard library; exits non-zero when a floor
is given (--floor-recall / --floor-mrr) and the metric falls below it. Without
a floor the run is advisory (the D16 baseline instrument).

Usage:
    python tools/evals/eval_search.py --cli cargo --workspace <dir> [--k 5] [--floor-recall 1.0]

The CLI command is invoked as: <cli> run -p momotaro-cli -- search <q> --json --top <k>
(executed from the repo root, with the search run inside <workspace>).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")


def run_search(cli_cmd: list[str], workspace: Path, query: str, k: int) -> list[dict]:
    cmd = [*cli_cmd, "search", query, "--json", "--top", str(k)]
    proc = subprocess.run(cmd, cwd=workspace, capture_output=True, text=True, encoding="utf-8")
    if proc.returncode != 0:
        raise RuntimeError(f"search failed for {query!r}: {proc.stderr.strip()}")
    payload = json.loads(proc.stdout)
    return payload.get("hits", [])


def evaluate(entries: list[dict], cli_cmd: list[str], workspace: Path, k: int) -> dict:
    recalls: list[float] = []
    rranks: list[float] = []
    per_query: list[dict] = []
    for entry in entries:
        hits = run_search(cli_cmd, workspace, entry["query"], k)
        got_keys = [hit.get("source_key", "") for hit in hits]
        expected = set(entry["expected"])
        found = sum(1 for e in expected if e in got_keys)
        recall = found / len(expected)
        rr = 0.0
        for rank, key in enumerate(got_keys, start=1):
            if key in expected:
                rr = 1.0 / rank
                break
        recalls.append(recall)
        rranks.append(rr)
        per_query.append(
            {"query": entry["query"], "recall": recall, "rr": rr, "top_keys": got_keys[:k]}
        )
    return {
        "k": k,
        "n": len(entries),
        "recall_at_k": sum(recalls) / len(recalls) if recalls else 0.0,
        "mrr": sum(rranks) / len(rranks) if rranks else 0.0,
        "per_query": per_query,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--golden", type=Path, default=Path(__file__).parent / "golden_set.json")
    parser.add_argument("--workspace", type=Path, required=True, help="initialized momotaro workspace dir")
    parser.add_argument("--cli", nargs="+", default=["cargo", "run", "-p", "momotaro-cli", "--"])
    parser.add_argument("--k", type=int, default=5)
    parser.add_argument("--floor-recall", type=float, default=None)
    parser.add_argument("--floor-mrr", type=float, default=None)
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()

    entries = json.loads(args.golden.read_text(encoding="utf-8"))["queries"]
    report = evaluate(entries, args.cli, args.workspace, args.k)

    if not args.quiet:
        print(f"queries: {report['n']}  k={report['k']}")
        print(f"recall@{report['k']}: {report['recall_at_k']:.4f}")
        print(f"mrr:       {report['mrr']:.4f}")
        print()
        for row in report["per_query"]:
            mark = "ok " if row["recall"] == 1.0 else "MISS"
            print(f"  [{mark}] {row['query']!r:30} recall={row['recall']:.2f} rr={row['rr']:.2f}")
            if row["recall"] < 1.0:
                print(f"         got: {row['top_keys']}")

    failed = False
    if args.floor_recall is not None and report["recall_at_k"] < args.floor_recall:
        print(f"FAIL: recall@{report['k']} {report['recall_at_k']:.4f} < floor {args.floor_recall}")
        failed = True
    if args.floor_mrr is not None and report["mrr"] < args.floor_mrr:
        print(f"FAIL: mrr {report['mrr']:.4f} < floor {args.floor_mrr}")
        failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
