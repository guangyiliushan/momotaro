#!/usr/bin/env python3
"""Regression tests for eval_search's malformed-input harness boundary."""

from __future__ import annotations

import argparse
import importlib.util
import json
import subprocess
import sys
import tempfile
from pathlib import Path


def load_eval_search():
    path = Path(__file__).with_name("eval_search.py")
    spec = importlib.util.spec_from_file_location("eval_search", path)
    if spec is None or spec.loader is None:
        raise AssertionError("eval_search.py is importable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


eval_search = load_eval_search()


def test_malformed_golden_entries_raise_value_error():
    for entries in ["not-a-list", [{}], [{"query": 7, "sufficient": []}], [{"query": "q", "sufficient": "one"}]]:
        try:
            eval_search.evaluate(entries, ["unused"], Path("."), 5, set())
        except ValueError:
            pass
        else:
            raise AssertionError(f"expected ValueError for {entries!r}")


def test_malformed_known_miss_is_rejected():
    bad = [{"query": "q", "sufficient": ["a"], "known_miss": {"reason": "typo", "why": "x"}}]
    try:
        eval_search.validate_entries(bad, "entries")
    except ValueError:
        pass
    else:
        raise AssertionError("expected ValueError for a bad known_miss reason")
    eval_search.validate_entries(
        [{"query": "q", "sufficient": ["a"], "known_miss": {"reason": "fused", "why": "x"}}],
        "entries",
    )


def test_miss_counts_split_unexpected_from_whitelisted():
    rows = [
        {"recall": 1.0, "known_miss": None},
        {"recall": 0.0, "known_miss": None},
        {"recall": 0.0, "known_miss": {"reason": "fused", "why": "w"}},
        {"recall": 0.5, "known_miss": {"reason": "displaced", "why": "w"}},
        {"recall": 0.5, "known_miss": {"reason": "displaced", "why": "w"}},
    ]
    counts = eval_search.miss_counts(rows)
    assert counts == {"unexpected": 1, "known": {"fused": 1, "displaced": 2}}, counts


def _report(**over):
    report = {
        "k": 5,
        "n": 1,
        "recall_at_k": 1.0,
        "mrr": 1.0,
        "hn_seats": {"semi_relevant": 0, "no_info": 0},
        "poison_seats": 0,
        "unexpected_misses": 0,
        "known_misses": {"fused": 0, "displaced": 0},
        "per_query": [],
    }
    report.update(over)
    return report


def _args(**over):
    values = {
        "floor_mrr": None,
        "floor_inj_mrr": None,
        "fail_on_unexpected_miss": False,
        "max_known_miss_fused": None,
        "max_known_miss_displaced": None,
        "max_hn_seats": None,
        "max_poison_seats": None,
        "max_inj_poison_seats": None,
    }
    values.update(over)
    return argparse.Namespace(**values)


def test_gate_failures():
    # A clean pair passes everything.
    assert eval_search.gate_failures(_report(), _report(), _args()) == []
    # An unwhitelisted miss only fails the strict gate, and only when asked.
    miss = _report(unexpected_misses=1)
    assert eval_search.gate_failures(miss, _report(), _args()) == []
    strict = eval_search.gate_failures(miss, _report(), _args(fail_on_unexpected_miss=True))
    assert len(strict) == 1 and "unexpected miss" in strict[0], strict
    # The whitelist itself is capped per reason, across both reports.
    grown = _report(known_misses={"fused": 3, "displaced": 1})
    caps = eval_search.gate_failures(
        grown, _report(), _args(max_known_miss_fused=2, max_known_miss_displaced=2)
    )
    assert len(caps) == 1 and "known-miss fused" in caps[0], caps
    # Floors and ceilings still bind.
    lines = eval_search.gate_failures(
        _report(mrr=0.5, hn_seats={"semi_relevant": 9, "no_info": 9}, poison_seats=3),
        _report(mrr=0.5, poison_seats=3),
        _args(
            floor_mrr=0.9,
            floor_inj_mrr=0.9,
            max_hn_seats=10,
            max_poison_seats=2,
            max_inj_poison_seats=2,
        ),
    )
    assert len(lines) == 5, lines


def test_malformed_golden_file_exits_three():
    with tempfile.TemporaryDirectory() as directory:
        golden = Path(directory) / "golden.json"
        golden.write_text(json.dumps({"queries": [{}]}), encoding="utf-8")
        proc = subprocess.run(
            [
                sys.executable,
                str(Path(__file__).with_name("eval_search.py")),
                "--golden",
                str(golden),
                "--workspace",
                directory,
                "--cli",
                "unused",
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
    assert proc.returncode == 3, proc.stdout + proc.stderr
    assert "HARNESS ERROR" in proc.stdout, proc.stdout


if __name__ == "__main__":
    test_malformed_golden_entries_raise_value_error()
    test_malformed_known_miss_is_rejected()
    test_miss_counts_split_unexpected_from_whitelisted()
    test_gate_failures()
    test_malformed_golden_file_exits_three()
    print("eval_search guard tests passed")
