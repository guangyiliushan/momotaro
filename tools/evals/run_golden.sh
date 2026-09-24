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
# unset-looking path, the repository itself, or a directory holding a checkout.
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
expected = {"ingest.started": 1, "ingest.finished": 1, "ingest.coverage": 6}
if counts != expected:
    print(f"FAIL: store_events {counts!r}, expected {expected!r}")
    sys.exit(1)
stub = connection.execute(
    "select count(*) from store_events where reason_code = 'icloud_placeholder'"
).fetchone()[0]
if stub != 1:
    print(f"FAIL: icloud_placeholder rows {stub!r}, expected 1 (D42)")
    sys.exit(1)
print(f"store_events ok: {counts} (+1 icloud_placeholder row)")
PY
"$python" "$repo/tools/evals/eval_search.py" \
  --workspace "$scratch" \
  --cli "$binary" \
  --k 5 \
  --floor-recall 0.95 \
  --floor-mrr 0.90
