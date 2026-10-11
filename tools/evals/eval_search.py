#!/usr/bin/env python3
"""Momotaro retrieval eval: runs `momotaro search --json` over the golden set.

Two reports over one deterministic run:

* report A (utility): recall@k and MRR on the `sufficient` sets — the minimal
  evidence set of source keys that answers a query. `recall = |sufficient ∩
  top-k sources| / |sufficient|`; `rr` is the reciprocal rank of the first
  sufficient hit.
* report B (robustness): the top-k seats taken by labeled negatives. Hard
  negatives (semi_relevant / no_info) are the per-query classes annotated in
  the golden set; `poison_keys` are the corpus-side attack samples and are
  counted separately for the main and injection query sets. Lower is better:
  these numbers measure how much of top-k the offenders own.

Gates are per-row, not on averages: a row scoring below 1.0 must carry
`known_miss` in the golden set (the explicit accepted-failure whitelist), and
the whitelist's size is itself gated per reason (`fused` / `displaced`), so a
new failure cannot hide inside a mean and a whitelist cannot quietly grow.

Pure standard library, zero LLM. Exits 0 on pass, 1 when a gate is given and
the metric crosses it (a metric regression), 3 when the harness itself fails
(a search crash or bad golden file — so CI can tell the two apart). Without a
gate the run is advisory (the baseline instrument).

Usage:
    python tools/evals/eval_search.py --workspace <dir> --cli <path/to/momotaro-cli[.exe>] \
        [--k 5] [--floor-mrr R] [--floor-inj-mrr R] [--fail-on-unexpected-miss] \
        [--max-known-miss-fused N] [--max-known-miss-displaced N] \
        [--max-hn-seats N] [--max-poison-seats N] [--max-inj-poison-seats N]

Floor/ceiling values live in run_golden.sh (with the measured baseline they
were set against), not here — this usage line does not repeat them.

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
    if not isinstance(payload, dict):
        raise ValueError(f"search returned a non-object payload for {query!r}")
    hits = payload.get("hits", [])
    if not isinstance(hits, list) or any(not isinstance(hit, dict) for hit in hits):
        raise ValueError(f"search returned malformed hits for {query!r}")
    return hits


def validate_entries(entries: object, label: str) -> None:
    """Reject malformed golden data before it can escape as KeyError/TypeError."""
    if not isinstance(entries, list):
        raise ValueError(f"{label} must be a list")
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise ValueError(f"{label}[{index}] must be an object")
        if not isinstance(entry.get("query"), str) or not entry["query"]:
            raise ValueError(f"{label}[{index}].query must be a non-empty string")
        sufficient = entry.get("sufficient")
        if (
            not isinstance(sufficient, list)
            or not sufficient
            or any(not isinstance(key, str) or not key for key in sufficient)
        ):
            raise ValueError(f"{label}[{index}].sufficient must be non-empty strings")
        negatives = entry.get("hard_negatives", [])
        if not isinstance(negatives, list):
            raise ValueError(f"{label}[{index}].hard_negatives must be a list")
        for position, label_row in enumerate(negatives):
            if (
                not isinstance(label_row, dict)
                or not isinstance(label_row.get("key"), str)
                or label_row.get("kind") not in ("semi_relevant", "no_info")
            ):
                raise ValueError(
                    f"{label}[{index}].hard_negatives[{position}] is malformed"
                )
        known = entry.get("known_miss")
        if known is not None and (
            not isinstance(known, dict)
            or known.get("reason") not in ("fused", "displaced")
            or not isinstance(known.get("why"), str)
            or not known["why"]
        ):
            raise ValueError(f"{label}[{index}].known_miss is malformed")


def evaluate(
    entries: list[dict], cli_cmd: list[str], workspace: Path, k: int, poison_keys: set[str]
) -> dict:
    """Utility + per-query seat counts for one entry list."""
    validate_entries(entries, "entries")
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
                "known_miss": entry.get("known_miss"),
            }
        )
    misses = miss_counts(per_query)
    return {
        "k": k,
        "n": len(entries),
        "recall_at_k": sum(recalls) / len(recalls) if recalls else 0.0,
        "mrr": sum(rranks) / len(rranks) if rranks else 0.0,
        "hn_seats": hn_seats,
        "poison_seats": poison_seats,
        "unexpected_misses": misses["unexpected"],
        "known_misses": misses["known"],
        "per_query": per_query,
    }


def miss_counts(per_query: list[dict]) -> dict:
    """Split sub-1.0 rows into unwhitelisted (unexpected) vs known, by reason.

    Pure: this is the accounting the gates read, kept out of the search loop
    so it can be tested without a CLI.
    """
    unexpected = 0
    known = {"fused": 0, "displaced": 0}
    for row in per_query:
        if row["recall"] >= 1.0:
            continue
        whitelisted = row.get("known_miss")
        if whitelisted is None:
            unexpected += 1
        else:
            known[whitelisted["reason"]] += 1
    return {"unexpected": unexpected, "known": known}


def print_report(title: str, report: dict) -> None:
    print(f"== {title} ==")
    print(f"queries: {report['n']}  k={report['k']}")
    print(f"recall@{report['k']}: {report['recall_at_k']:.4f}")
    print(f"mrr:       {report['mrr']:.4f}")
    print()
    for row in report["per_query"]:
        if row["recall"] == 1.0:
            mark = "ok  "
        elif row["known_miss"] is not None:
            mark = "KNOWN " + row["known_miss"]["reason"]
        else:
            mark = "MISS"
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
            if row["known_miss"] is not None:
                print(f"         why: {row['known_miss']['why']}")
    known = report["known_misses"]
    print(
        f"  misses: {report['unexpected_misses']} unexpected,"
        f" {known['fused']} known-fused, {known['displaced']} known-displaced"
    )
    print()


def gate_failures(main_report: dict, inj_report: dict, args) -> list[str]:
    """Gate lines to fail on; empty means pass. Pure — tested without a CLI.

    MRR floors are tripwires on ranking. Hits are gated per row instead of by
    mean: `--fail-on-unexpected-miss` refuses any sub-1.0 row outside the
    known_miss whitelist, and the whitelist's own size is capped per reason so
    accepted failures cannot quietly grow.
    """
    lines: list[str] = []

    def floor(name: str, value: float | None, actual: float) -> None:
        if value is not None and actual < value:
            lines.append(f"FAIL: {name} {actual:.4f} < floor {value}")

    def ceiling(name: str, value: int | None, actual: int) -> None:
        if value is not None and actual > value:
            lines.append(f"FAIL: {name} {actual} > ceiling {value}")

    floor("mrr", args.floor_mrr, main_report["mrr"])
    floor("injection mrr", args.floor_inj_mrr, inj_report["mrr"])
    unexpected = main_report["unexpected_misses"] + inj_report["unexpected_misses"]
    if args.fail_on_unexpected_miss and unexpected:
        lines.append(
            f"FAIL: {unexpected} unexpected miss(es) — sub-1.0 rows outside the known_miss whitelist"
        )
    fused = main_report["known_misses"]["fused"] + inj_report["known_misses"]["fused"]
    displaced = main_report["known_misses"]["displaced"] + inj_report["known_misses"]["displaced"]
    ceiling("known-miss fused (both sets)", args.max_known_miss_fused, fused)
    ceiling("known-miss displaced (both sets)", args.max_known_miss_displaced, displaced)
    hn_total = main_report["hn_seats"]["semi_relevant"] + main_report["hn_seats"]["no_info"]
    ceiling("hard-negative seats (main)", args.max_hn_seats, hn_total)
    ceiling("poison seats (main)", args.max_poison_seats, main_report["poison_seats"])
    ceiling("poison seats (injection)", args.max_inj_poison_seats, inj_report["poison_seats"])
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--golden", type=Path, default=Path(__file__).parent / "golden_set.json")
    parser.add_argument("--workspace", type=Path, required=True, help="initialized momotaro workspace dir")
    parser.add_argument("--cli", nargs="+", default=["cargo", "run", "-p", "momotaro-cli", "--"])
    parser.add_argument("--k", type=int, default=5)
    parser.add_argument("--floor-mrr", type=float, default=None)
    parser.add_argument("--floor-inj-mrr", type=float, default=None)
    parser.add_argument("--fail-on-unexpected-miss", action="store_true")
    parser.add_argument("--max-known-miss-fused", type=int, default=None)
    parser.add_argument("--max-known-miss-displaced", type=int, default=None)
    parser.add_argument("--max-hn-seats", type=int, default=None)
    parser.add_argument("--max-poison-seats", type=int, default=None)
    parser.add_argument("--max-inj-poison-seats", type=int, default=None)
    args = parser.parse_args()

    try:
        golden = json.loads(args.golden.read_text(encoding="utf-8"))
        if not isinstance(golden, dict):
            raise ValueError("golden file must be an object")
        for field in ("queries", "injection"):
            validate_entries(golden.get(field), field)
        poison_values = golden.get("poison_keys", [])
        if not isinstance(poison_values, list) or any(
            not isinstance(key, str) or not key for key in poison_values
        ):
            raise ValueError("golden.poison_keys must be non-empty strings")
        poison_keys = set(poison_values)
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

    failures = gate_failures(main_report, inj_report, args)
    for line in failures:
        print(line)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
