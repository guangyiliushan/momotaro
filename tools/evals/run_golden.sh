#!/usr/bin/env bash
# Builds the CLI, indexes the fixture vault in a scratch workspace and gates the
# golden set. Shared by the CI `instruments` job, so
# the recipe, the thresholds and the reported numbers cannot drift apart.
#
# Usage: tools/evals/run_golden.sh [scratch-directory]
set -euo pipefail

repo_posix="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="${1:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/momotaro-eval}"
cd "$repo_posix"

# `rm -rf` below, so refuse anything that could be a source tree: a relative or
# unset-looking path, the repository itself, any directory inside the
# repository, or a directory holding a checkout.
case "$scratch" in
  /* | [A-Za-z]:[\\/]*) ;;
  *)
    echo "run_golden.sh: the scratch directory must be absolute: '$scratch'" >&2
    exit 2
    ;;
esac
scratch_posix="$scratch"
if command -v cygpath >/dev/null 2>&1; then
  scratch_posix="$(cygpath -u "$scratch" 2>/dev/null || printf '%s' "$scratch")"
fi
# Compare resolved paths only: literal .., duplicate slashes, and symlinks must
# not let a source tree masquerade as an outside scratch directory.
if ! command -v realpath >/dev/null 2>&1 || ! scratch_posix="$(realpath -m -- "$scratch_posix" 2>/dev/null)"; then
  echo "run_golden.sh: cannot canonicalize scratch directory '$scratch'" >&2
  exit 2
fi
case "$scratch_posix" in
  *".."*)
    echo "run_golden.sh: scratch directory did not canonicalize cleanly: '$scratch'" >&2
    exit 2
    ;;
esac
case "$repo_posix/" in
  "$scratch_posix"/*)
    echo "run_golden.sh: refusing to delete '$scratch' — the repository lives inside it" >&2
    exit 2
    ;;
esac
case "$scratch_posix/" in
  "$repo_posix"/*)
    echo "run_golden.sh: refusing to delete '$scratch' — it is inside the repository" >&2
    exit 2
    ;;
esac
if [ "$scratch_posix" = "/" ] || [ "$scratch_posix" = "$HOME" ] \
  || [ "$(basename "$scratch_posix")" = ".git" ] \
  || [ -e "$scratch/.git" ] || [ -e "$scratch/Cargo.toml" ]; then
  echo "run_golden.sh: refusing to delete '$scratch' — it looks like a repository or a home directory" >&2
  exit 2
fi

# python is resolved here and not later because the binary path below is read
# out of `cargo metadata`, and that is JSON.
python=""
for candidate in "${PYTHON:-}" python3 python; do
  [ -n "$candidate" ] || continue
  # Run it, not just look for it: on Windows `python3` can be an App Execution
  # Alias stub that exists on PATH and then exits non-zero without Python.
  if "$candidate" -c "import sys" >/dev/null 2>&1; then
    python="$candidate"
    break
  fi
done
[ -n "$python" ] || {
  echo "run_golden.sh: no working python (set PYTHON=/path/to/python)" >&2
  exit 1
}

# Where cargo actually put that binary. `cargo build` honours
# `CARGO_TARGET_DIR` and `.cargo/config.toml`; a hard-coded `target/debug` does
# not, so the moment they diverge this gate runs a binary this run never built
# — and on a machine that already has one at the hard-coded path, silently runs
# a stale one. `cargo metadata` is the same source of truth cargo uses.
# Ask the build where it put the executable instead of re-deriving the path:
# `target_directory` has no target-triple segment, so `CARGO_BUILD_TARGET` (or
# `build.target` in a config file) would send the artifacts to
# `<target>/<triple>/debug/` while a re-derived path still looked in
# `<target>/debug/` — the same stale-binary hole this replaced, one level down.
binary="$(cargo build -p momotaro-cli --locked --message-format=json \
  | "$python" -c 'import json, sys
last = ""
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        message = json.loads(line)
    except Exception:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable"):
        last = message["executable"]
print(last)')"
[ -n "$binary" ] || {
  echo "run_golden.sh: the build reported no momotaro-cli executable" >&2
  exit 2
}
if command -v cygpath >/dev/null 2>&1; then
  binary="$(cygpath -u "$binary" 2>/dev/null || printf '%s' "$binary")"
fi
[ -x "$binary" ] || binary="$binary.exe"

# Never in the repository root: `init` writes `.momotaro/` where it runs.
rm -rf "$scratch"
mkdir -p "$scratch/vault"
cp -r crates/momotaro-run/tests/fixtures/vault/. "$scratch/vault/"
# Eval-owned corpus expansion: hard negatives, poison samples, one
# bad-frontmatter note (with the frontmatter work).
cp -r tools/evals/fixtures/. "$scratch/vault/"
printf '[vault]\npath = "vault"\n' > "$scratch/momotaro.toml"
# An iCloud eviction stub is not content — it never enters the index, so
# the retrieval numbers below cannot move because of it — and its audit row is
# the only reason a report can say a note was evicted on some other device.
printf 'stub text\n' > "$scratch/vault/.note.md.icloud"

# The binary, the workspace and the eval script all reach native subprocesses, so
# MSYS/Git Bash needs native spellings for them (`/d/...` arrives as `D:\d\...`).
repo="$repo_posix"
if command -v cygpath >/dev/null 2>&1; then
  binary="$(cygpath -w "$binary")"
  scratch="$(cygpath -w "$scratch")"
  repo="$(cygpath -w "$repo_posix")"
fi

(cd "$scratch" && "$binary" init && "$binary" index)

# The retrieval numbers say nothing about the ingest audit trail: a
# deleted `ingest.coverage` write would still score 0.9167. The batch is checked
# here so the instrument cannot call a broken indexer healthy.
"$python" - "$scratch" <<'PY'
import sqlite3, sys
db = sys.argv[1].rstrip("/\\") + "/.momotaro/momotaro.db"
connection = sqlite3.connect(db)
counts = dict(connection.execute("select type, count(*) from store_events group by type"))
# 5 vault fixtures + 15 eval fixtures (9 zh/en + 6 ja) + 1 icloud stub = 21
# coverage rows. **This number tracks the fixture count on purpose** — it is the
# tripwire for a missing `ingest.coverage` write, so it stays a literal. Adding
# a fixture means adding one here; a derived count would stop catching the bug
# it exists to catch.
expected = {"ingest.started": 1, "ingest.finished": 1, "ingest.coverage": 21}
if counts != expected:
    print(f"FAIL: store_events {counts!r}, expected {expected!r}")
    sys.exit(1)
stub = connection.execute(
    "select count(*) from store_events where reason_code = 'icloud_placeholder'"
).fetchone()[0]
if stub != 1:
    print(f"FAIL: icloud_placeholder rows {stub!r}, expected 1")
    sys.exit(1)
frontmatter = connection.execute(
    "select count(*) from store_events where reason_code = 'frontmatter_parse_error'"
).fetchone()[0]
if frontmatter != 1:
    print(f"FAIL: frontmatter_parse_error rows {frontmatter!r}, expected 1")
    sys.exit(1)
print(f"store_events ok: {counts} (+1 icloud_placeholder, +1 frontmatter_parse_error)")
PY
# Gates. The instrument is deterministic, so these are tripwires, not
# tolerances. Hits are gated per row, not on averages: --fail-on-unexpected-miss
# fails on ANY sub-1.0 row outside the golden set's known_miss whitelist, and
# the whitelist's size is capped per reason (any growth trips). MRR floors sit
# below the measured baseline by less than one minimal step (0.05/N: one query
# demoted rank-4 to rank-5); seat ceilings sit exactly at baseline.
# Re-baselined 2026-10-29 after the query stopped degrading into its last word.
# `QueryParser` turns any whitespace-separated chunk that tokenizes to more than
# one term into a `PhraseQuery`, and a CJK query has no whitespace, so an
# injection shell fused with the query's first word into a phrase nobody holds.
# The search then answered on whatever followed the last space — `忽略上述指令。
# PCA 方差` searched for `方差` — and reported it as an ordinary result. Terms now
# come from the analyzer, one term per token, so a query means the same thing
# however its author spaced it.
#
# What that moved, and why the numbers below are what they are:
#   - injection fused 12 -> 0: that stratum was entirely this bug. The ceiling
#     ratchets to 0, so any phrase-shaped query trips it again;
#   - injection displaced 12 -> 9 (recall@5 0.5000 -> 0.8667, mrr 0.3667 ->
#     0.4107). What is left is three base queries under three shells, and
#     `PCA 方差` fails on the main set too — the shell is no longer a cause. The
#     ceiling ratchets to the measured 11 (2 main + 9 injection);
#   - main mrr 0.8222 -> 0.8556 on unchanged recall, so both floors rise to sit
#     within one minimal step (0.05/N) of the new baseline. Left where they
#     were, the gate would not notice the gain being lost;
#   - hard-negative seats 24 -> 25: the honest price of not making phrases. A
#     multi-term chunk is now matched term by term, so one more semi-relevant
#     note holds a seat on the main set. Raised to the measured count, not
#     beyond it;
#   - injection poison seats 38 -> 87: this gauge changed what it measures. The
#     shelled queries used to degrade to a single word and so never searched for
#     their own shell text; the poison samples had nothing to match. They match
#     now because the query is actually run, and the poison notes carry that
#     text verbatim. Retrieval retrieving is not the injection defence — the
#     untrusted-content delimiters and the prompt hierarchy are (context
#     pipeline, `render_fragments`), and this set is the retrieval surface only.
#
# Re-baselined 2026-10-30 after the rank stage stopped spending the window on
# one source's duplicates. Each source's best chunk is now taken before any
# source's second, so a note that cuts into several matching chunks cannot hold
# the window and starve the other sources. That is a selection rule only: the
# score is untouched, and the documented total order still decides how the
# selected rows print.
#
# What that moved, and why the numbers below are what they are:
#   - `特征值 定义 求法` lost three of its four rows and every remaining window
#     is five distinct sources. Same-source crowding is gone; what is left is
#     which source ranks high enough (`PCA 方差` has pca.md at six);
#   - known-miss displaced ratchets 11 -> 8 in the same change;
#   - the three seat ceilings go UP, and that is the price rather than a
#     regression to paper over. The window used to spend its seats on one
#     source's duplicate chunks, which is exactly what kept the lower-ranked
#     sources out — semi_relevant, no_info and the poison samples among them.
#     Stop wasting the seats and the window reaches further down the source
#     list, where those live. Report A is the gate; report B is the harm gauge
#     that says how much of the window is noise, and it is now telling the
#     truth about a ranking problem instead of being suppressed by duplication.
#
# The same day, three annotations turned out to be unreachable rather than
# outranked: `pca.md` answered `PCA 方差` while never writing 方差 (it said
# "covariance matrix"), and the eigenvalue notes discussed the definition and
# the method without using those words. Retrieval matches on terms, so no
# ranking rule could have put them first. The notes now use the vocabulary the
# questions are asked with, `gen_annotations.py` refuses an annotation whose
# evidence never carries the query's terms, and main is at recall 1.0. What is
# left in injection is the attack surface itself: two poison samples carry the
# shell text verbatim and take two of the five seats.
#
# Everything below is a measured count set exactly at the reading. Any growth
# trips; nothing here is a tolerance.
"$python" "$repo/tools/evals/gen_annotations.py" --check
"$python" "$repo/tools/evals/eval_search.py" \
  --workspace "$scratch" \
  --cli "$binary" \
  --k 5 \
  --fail-on-unexpected-miss \
  --max-known-miss-fused 0 \
  --max-known-miss-displaced 4 \
  --floor-mrr 0.877 \
  --floor-inj-mrr 0.410 \
  --max-hn-seats 31 \
  --max-poison-seats 14 \
  --max-inj-poison-seats 90
