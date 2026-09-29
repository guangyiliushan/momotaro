#!/usr/bin/env bash
# Builds the CLI, indexes the fixture vault in a scratch workspace and gates the
# golden set. Shared by `tools/evals/README.md` and the CI `instruments` job, so
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

cargo build -p momotaro-cli --locked
binary="$repo_posix/target/debug/momotaro-cli"
[ -x "$binary" ] || binary="$binary.exe"

# Never in the repository root: `init` writes `.momotaro/` where it runs.
rm -rf "$scratch"
mkdir -p "$scratch/vault"
cp -r crates/momotaro-run/tests/fixtures/vault/. "$scratch/vault/"
# Eval-owned corpus expansion: hard negatives, poison samples, one
# bad-frontmatter note (W2.3 handover). Inventory and roles: tools/evals/README.md.
cp -r tools/evals/fixtures/. "$scratch/vault/"
printf '[vault]\npath = "vault"\n' > "$scratch/momotaro.toml"
# D42: an iCloud eviction stub is not content — it never enters the index, so
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

(cd "$scratch" && "$binary" init && "$binary" index)

# The retrieval numbers say nothing about the ingest audit trail (ADR 0026): a
# deleted `ingest.coverage` write would still score 0.9583. The batch is checked
# here so the instrument cannot call a broken indexer healthy.
"$python" - "$scratch" <<'PY'
import sqlite3, sys
db = sys.argv[1].rstrip("/\\") + "/.momotaro/momotaro.db"
connection = sqlite3.connect(db)
counts = dict(connection.execute("select type, count(*) from store_events group by type"))
# 5 vault fixtures + 7 eval fixtures + 1 icloud stub = 13 coverage rows.
expected = {"ingest.started": 1, "ingest.finished": 1, "ingest.coverage": 13}
if counts != expected:
    print(f"FAIL: store_events {counts!r}, expected {expected!r}")
    sys.exit(1)
stub = connection.execute(
    "select count(*) from store_events where reason_code = 'icloud_placeholder'"
).fetchone()[0]
if stub != 1:
    print(f"FAIL: icloud_placeholder rows {stub!r}, expected 1 (D42)")
    sys.exit(1)
frontmatter = connection.execute(
    "select count(*) from store_events where reason_code = 'frontmatter_parse_error'"
).fetchone()[0]
if frontmatter != 1:
    print(f"FAIL: frontmatter_parse_error rows {frontmatter!r}, expected 1 (D41.5)")
    sys.exit(1)
print(f"store_events ok: {counts} (+1 icloud_placeholder, +1 frontmatter_parse_error)")
PY
# Report A floors and report B ceilings. The instrument is deterministic, so
# these are tripwires, not tolerances: each floor sits below the measured
# baseline by less than one minimal step, each ceiling sits exactly at it, so
# any real regression trips it. Re-baselined 2026-09-26 on the expanded corpus
# (post data-quality fix — `特征值 特征向量` sufficiency re-anchored on the
# minimal set):
#   - main recall@5 0.9583 -> 0.9167: hard negatives and poison samples now
#     compete for top-5 seats (`PCA 方差` lost pca.md from top-5 entirely —
#     documented hard-negative harm, not a retrieval change);
#   - main mrr 0.9028 -> 0.8333: same cause;
#   - injection numbers are the first measurement of this subset — see
#     README.md for the two zero-recall shapes (whitespace-free queries hit as
#     one unmatchable phrase; and hard negatives displacing pca.md). The
#     subset's utility baseline starts low and honest;
#   - seat counts (19 / 11 / 33) are exact; any growth trips.
# Minimal steps (all sufficient sets are single-key now): recall 1/12 and
# 1/36; mrr 0.05/12 and 0.05/36 (one query demoted rank-4 to rank-5).
"$python" "$repo/tools/evals/gen_annotations.py" --check
"$python" "$repo/tools/evals/eval_search.py" \
  --workspace "$scratch" \
  --cli "$binary" \
  --k 5 \
  --floor-recall 0.91 \
  --floor-mrr 0.833 \
  --floor-inj-recall 0.49 \
  --floor-inj-mrr 0.351 \
  --max-hn-seats 19 \
  --max-poison-seats 11 \
  --max-inj-poison-seats 33
