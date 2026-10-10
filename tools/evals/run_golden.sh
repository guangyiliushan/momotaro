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

(cd "$scratch" && "$binary" init && "$binary" index)

# The retrieval numbers say nothing about the ingest audit trail (ADR 0026): a
# deleted `ingest.coverage` write would still score 0.9167. The batch is checked
# here so the instrument cannot call a broken indexer healthy.
"$python" - "$scratch" <<'PY'
import sqlite3, sys
db = sys.argv[1].rstrip("/\\") + "/.momotaro/momotaro.db"
connection = sqlite3.connect(db)
counts = dict(connection.execute("select type, count(*) from store_events group by type"))
# 5 vault fixtures + 9 eval fixtures + 1 icloud stub = 15 coverage rows.
expected = {"ingest.started": 1, "ingest.finished": 1, "ingest.coverage": 15}
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
# Gates. The instrument is deterministic, so these are tripwires, not
# tolerances. Hits are gated per row, not on averages: --fail-on-unexpected-miss
# fails on ANY sub-1.0 row outside the golden set's known_miss whitelist, and
# the whitelist's size is capped per reason (any growth trips). MRR floors sit
# below the measured baseline by less than one minimal step (0.05/N: one query
# demoted rank-4 to rank-5); seat ceilings sit exactly at baseline.
# Re-baselined 2026-10-09 on the grown golden set (15 main / 45 injection;
# eigenvalue.md + kernel-def.md added, `特征值 特征向量` / `kernel` re-anchored,
# three multi-key sufficient sets):
#   - main recall@5 0.9167 -> 0.9000 / mrr 0.8333 -> 0.8222: the two accepted
#     displaced rows (`PCA 方差`, `特征值 定义 求法`) are whitelisted by name
#     now — the average can no longer absorb a new miss;
#   - injection: 12 fused (whitespace-free base queries; analyzer phrase
#     fusion) + 12 displaced rows whitelisted; recall@5 0.5000 / mrr 0.3667
#     stay the honest low baseline, but any NEW failure trips;
#   - seat counts (24 / 8 / 38) are exact; any growth trips.
"$python" "$repo/tools/evals/gen_annotations.py" --check
"$python" "$repo/tools/evals/eval_search.py" \
  --workspace "$scratch" \
  --cli "$binary" \
  --k 5 \
  --fail-on-unexpected-miss \
  --max-known-miss-fused 12 \
  --max-known-miss-displaced 14 \
  --floor-mrr 0.822 \
  --floor-inj-mrr 0.366 \
  --max-hn-seats 24 \
  --max-poison-seats 8 \
  --max-inj-poison-seats 38
