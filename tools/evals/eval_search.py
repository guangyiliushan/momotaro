#!/usr/bin/env python3
"""Momotaro retrieval eval: runs `momotaro search --json` over the golden set.

Two reports over one deterministic run (docs/docs/context-retrieval.md §3):

* report A (utility): recall@k and MRR on the `sufficient` sets — the minimal
  evidence set of source keys that answers a query. `recall = |sufficient ∩
  top-k sources| / |sufficient|`; `rr` is the reciprocal rank of the first
  sufficient hit.
* report B (robustness): the top-k seats taken by labeled negatives. Hard
  negatives (semi_relevant / no_info) are the per-query classes annotated in
  the golden set; `poison_keys` are the corpus-side attack samples and are
  counted separately for the main and injection query sets. Lower is better:
  these numbers measure how much of top-k the offenders own.

Pure standard library, zero LLM. Exits 0 on pass, 1 when a gate is given and
the metric crosses it (a metric regression), 3 when the harness itself fails
(a search crash or bad golden file — so CI can tell the two apart). Without a
gate the run is advisory (the D16 baseline instrument).

Usage:
    python tools/evals/eval_search.py --workspace <dir> --cli <path/to/momotaro-cli[.exe>] \
        [--k 5] [--floor-recall R] [--floor-mrr R] [--floor-inj-recall R] \
        [--floor-inj-mrr R] [--max-hn-seats N] [--max-poison-seats N] \
        [--max-inj-poison-seats N]

Floor/ceiling values live in run_golden.sh (with the measured baseline they
were set against), not here — this usage line does not repeat them. ...

`--cli` is the *command prefix* that runs the binary — a path here, but the
default is `cargo run -p momotaro-cli --` — and each query is executed as
`<cli...> search <query> --json --top <k>` with the workspace as the working
directory. Give it explicitly for a workspace that is not a cargo project.
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


def evaluate(
    entries: list[dict], cli_cmd: list[str], workspace: Path, k: int, poison_keys: set[str]
) -> dict:
    """Utility + per-query seat counts for one entry list."""
    recalls: list[float] = []
    rranks: list[float] = []
    hn_seats = {"semi_relevant": 0, "no_info": 0}
    poison_seats = 0
    per_query: list[dict] = []
    for entry in entries:
        hits = run_search(cli_cmd, workspace, entry["query"], k)
        got_keys = [hit.get("source_key", "") for hit in hits]
        sufficient = set(entry["sufficient"])
        found = sum(1 for e in sufficient if e in got_keys)
        recall = found / len(sufficient)
        rr = 0.0
        for rank, key in enumerate(got_keys, start=1):
            if key in sufficient:
                rr = 1.0 / rank
                break
        # Seats, not distinct sources: every top-k slot an offender owns is a
        # slot a sufficient chunk did not get. One source can own several
        # slots — the count is over hits, not over distinct keys.
        row_hn = {"semi_relevant": 0, "no_info": 0}
        for label in entry.get("hard_negatives", []):
            row_hn[label["kind"]] += got_keys.count(label["key"])
        row_poison = sum(got_keys.count(key) for key in poison_keys)
        for kind, count in row_hn.items():
            hn_seats[kind] += count
        poison_seats += row_poison
        recalls.append(recall)
        rranks.append(rr)
        per_query.append(
            {
                "query": entry["query"],
                "recall": recall,
                "rr": rr,
                "hn_seats": row_hn,
                "poison_seats": row_poison,
                "top_keys": got_keys[:k],
            }
        )
    return {
        "k": k,
        "n": len(entries),
        "recall_at_k": sum(recalls) / len(recalls) if recalls else 0.0,
        "mrr": sum(rranks) / len(rranks) if rranks else 0.0,
        "hn_seats": hn_seats,
        "poison_seats": poison_seats,
        "per_query": per_query,
    }


def print_report(title: str, report: dict) -> None:
    print(f"== {title} ==")
    print(f"queries: {report['n']}  k={report['k']}")
    print(f"recall@{report['k']}: {report['recall_at_k']:.4f}")
    print(f"mrr:       {report['mrr']:.4f}")
    print()
    for row in report["per_query"]:
        mark = "ok " if row["recall"] == 1.0 else "MISS"
        extras = []
        if sum(row["hn_seats"].values()):
            extras.append(
                f"hn=semi:{row['hn_seats']['semi_relevant']}+no_info:{row['hn_seats']['no_info']}"
            )
        if row["poison_seats"]:
            extras.append(f"poison={row['poison_seats']}")
        extra = ("  " + " ".join(extras)) if extras else ""
        print(f"  [{mark}] {row['query']!r:30} recall={row['recall']:.2f} rr={row['rr']:.2f}{extra}")
        if row["recall"] < 1.0:
            print(f"         got: {row['top_keys']}")
    print()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--golden", type=Path, default=Path(__file__).parent / "golden_set.json")
    parser.add_argument("--workspace", type=Path, required=True, help="initialized momotaro workspace dir")
    parser.add_argument("--cli", nargs="+", default=["cargo", "run", "-p", "momotaro-cli", "--"])
    parser.add_argument("--k", type=int, default=5)
    parser.add_argument("--floor-recall", type=float, default=None)
    parser.add_argument("--floor-mrr", type=float, default=None)
    parser.add_argument("--floor-inj-recall", type=float, default=None)
    parser.add_argument("--floor-inj-mrr", type=float, default=None)
    parser.add_argument("--max-hn-seats", type=int, default=None)
    parser.add_argument("--max-poison-seats", type=int, default=None)
    parser.add_argument("--max-inj-poison-seats", type=int, default=None)
    args = parser.parse_args()

    try:
        golden = json.loads(args.golden.read_text(encoding="utf-8"))
        poison_keys = set(golden.get("poison_keys", []))
        main_report = evaluate(golden["queries"], args.cli, args.workspace, args.k, poison_keys)
        inj_report = evaluate(golden["injection"], args.cli, args.workspace, args.k, poison_keys)
    except (RuntimeError, ValueError, OSError) as exc:
        # Harness failure, not a metric regression: distinct exit code so CI
        # triage does not mistake a crashed search or bad golden file for a
        # tripped gate.
        print(f"HARNESS ERROR: {exc}")
        return 3

    print_report("report A: utility (main set)", main_report)
    print_report("report A: utility (injection subset)", inj_report)
    hn = main_report["hn_seats"]
    print("== report B: robustness (top-k seats owned by offenders) ==")
    print(
        f"hard-negative seats (main):       {hn['semi_relevant'] + hn['no_info']}"
        f"  (semi_relevant {hn['semi_relevant']}, no_info {hn['no_info']})"
    )
    print(f"poison seats (main):              {main_report['poison_seats']}")
    print(f"poison seats (injection subset):  {inj_report['poison_seats']}")
    print()

    failed = False

    def floor(name: str, value: float | None, actual: float) -> None:
        nonlocal failed
        if value is not None and actual < value:
            print(f"FAIL: {name} {actual:.4f} < floor {value}")
            failed = True

    def ceiling(name: str, value: int | None, actual: int) -> None:
        nonlocal failed
        if value is not None and actual > value:
            print(f"FAIL: {name} {actual} > ceiling {value}")
            failed = True

    floor("recall@%d" % args.k, args.floor_recall, main_report["recall_at_k"])
    floor("mrr", args.floor_mrr, main_report["mrr"])
    floor("injection recall@%d" % args.k, args.floor_inj_recall, inj_report["recall_at_k"])
    floor("injection mrr", args.floor_inj_mrr, inj_report["mrr"])
    hn_total = main_report["hn_seats"]["semi_relevant"] + main_report["hn_seats"]["no_info"]
    ceiling("hard-negative seats (main)", args.max_hn_seats, hn_total)
    ceiling("poison seats (main)", args.max_poison_seats, main_report["poison_seats"])
    ceiling("poison seats (injection)", args.max_inj_poison_seats, inj_report["poison_seats"])
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
