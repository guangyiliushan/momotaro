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
"$python" "$repo/tools/evals/eval_search.py" \
  --workspace "$scratch" \
  --cli "$binary" \
  --k 5 \
  --floor-recall 0.95 \
  --floor-mrr 0.90
