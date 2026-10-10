#!/usr/bin/env bash
# The gate suite for this repository, in the tree so a commit message that cites
# it can be replayed by anyone who has the checkout.
#
#   bash tools/gates.sh                  # close-out tier: fmt, clippy, doc, tests, ts, guards, golden, linux
#   bash tools/gates.sh --fast           # close-out tier, skipping the golden set
#   bash tools/gates.sh --task <crate>…  # daily tier: fmt + clippy/test for the named crates only
#   bash tools/gates.sh --only <step>    # one step, for CI's per-step visibility
#
# Pick the tier by BLAST RADIUS, not by habit: the daily tier is for edits whose
# reach is the crates you name; the close-out tier runs before a task commit, a
# wave merge, or any key / path / schema / format change. Name the crates whose
# behaviour the change can reach — not the crate whose file changed (contracts
# enums, `indexed_text`, key shapes go straight to the close-out tier).
#
# CI runs this same file, one `--only` per step, so the suite a contributor
# replays locally is the suite the merge gate runs. Nothing below is a
# re-derivation of what CI does; CI only chooses which step to ask for.
#
# The close-out tier exists to get three things right:
#   * every step's EXIT CODE is captured without a pipeline (`cargo … | tail`
#     reports tail's status, so a red clippy prints OK);
#   * `CARGO_TARGET_DIR` is unset before anything builds — a polluted value
#     silently builds elsewhere and the CLI binary in the repo stays stale;
#   * the test step runs SERIAL on Windows. A parallel run hits the registered
#     tantivy flake (permission denied / directory-not-empty on index builds,
#     load-sensitive, roughly 1 in 8) on tests unrelated to whatever changed;
#     serial is what the CI windows leg does. This is stabilising, not a fix: the
#     flake itself stays on the books.
#
# Review copies export their own `CARGO_TARGET_DIR` so dependency crates are
# compiled once and reused across copies — that is a workflow choice, not part
# of this script.
set -uo pipefail

unset CARGO_TARGET_DIR
cd "$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "not inside a git repository"; exit 2; }

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) export RUST_TEST_THREADS=1 ;;
esac

log="${TMPDIR:-/tmp}/momotaro-gates.log"
: > "$log"
fail=0

step() {
  local name="$1"; shift
  printf '\n=== %s ===\n' "$name" | tee -a "$log"
  "$@" >> "$log" 2>&1
  local code=$?
  if [ "$code" -eq 0 ]; then
    echo "OK   $name"
  else
    echo "FAIL $name (exit $code)"
    fail=1
  fi
}

run_fmt()    { step "fmt"    cargo fmt --all --check; }
run_clippy() { step "clippy" cargo clippy --workspace --all-targets --locked -- -D warnings; }
run_doc()    { step "doc"    env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked; }
run_test()   { step "test"   cargo test --workspace --locked; }
# The TypeScript surface. The root's `pnpm test` is `turbo run test` across
# every package that declares one — including `@momotaro/cli`, whose `test`
# shells out to `cargo` — so the step names out that one package and takes
# the rest: every `test` script that is TypeScript, now and as the workspace
# grows, without a list that goes stale when a package is added. A missing
# pnpm is a **red**, not a skip: the merge gate runs this step, so a local
# green that quietly passed over it would be exactly the silent-pass the
# exit-code rule above exists to prevent.
run_ts() {
  if command -v pnpm >/dev/null 2>&1; then
    step "ts" pnpm turbo run test --filter='!@momotaro/cli'
  else
    printf '\n=== ts ===\n' | tee -a "$log"
    echo "FAIL ts: pnpm is not on PATH (corepack enable, or npm i -g pnpm)" | tee -a "$log"
    fail=1
  fi
}

run_guards() { step "guards" bash tools/evals/test_guards.sh; }
run_golden() { step "golden" bash tools/evals/run_golden.sh; }

# Lints that only the ubuntu leg sees: platform-gated code hides them from a
# Windows host. Pure-Rust crates only — store/retrieve/run cannot cross-compile
# past rusqlite's bundled C.
run_linux() {
  if command -v rustup >/dev/null 2>&1 && rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-linux-gnu; then
    step "clippy (linux cfg)" cargo clippy -p momotaro-contracts -p momotaro-ingest \
      --all-targets --target x86_64-unknown-linux-gnu --locked -- -D warnings
  else
    echo "note: install the linux target for the cfg-gated lint check:"
    echo "      rustup target add x86_64-unknown-linux-gnu"
  fi
}

report() {
  echo
  if [ "$fail" -eq 0 ]; then
    echo "gates green (log: $log)"
  else
    echo "gates FAILED (log: $log)"
  fi
  exit "$fail"
}

case "${1:-}" in

  # One named step — what CI asks for, so the suite stays single-sourced.
  --only)
    case "${2:-}" in
      fmt)    run_fmt ;;
      clippy) run_clippy ;;
      doc)    run_doc ;;
      test)   run_test ;;
      ts)     run_ts ;;
      guards) run_guards ;;
      golden) run_golden ;;
      linux)  run_linux ;;
      *) echo "usage: bash tools/gates.sh --only fmt|clippy|doc|test|ts|guards|golden|linux"; exit 2 ;;
    esac
    report
    ;;

  # Daily tier: reach limited to the crates you name.
  --task)
    shift
    if [ $# -eq 0 ]; then
      echo "usage: bash tools/gates.sh --task <crate> [<crate>…]"
      exit 2
    fi
    pkgs=()
    for crate in "$@"; do pkgs+=(-p "$crate"); done
    step "fmt"           cargo fmt --all --check
    step "clippy (task)" cargo clippy "${pkgs[@]}" --all-targets --locked -- -D warnings
    step "test (task)"   cargo test "${pkgs[@]}" --locked
    echo
    if [ "$fail" -eq 0 ]; then
      echo "task tier green (log: $log)"
    else
      echo "task tier FAILED (log: $log)"
    fi
    exit "$fail"
    ;;

  # Close-out tier.
  *)
    run_fmt
    run_clippy
    run_doc
    run_test
    run_ts
    run_guards
    if [ "${1:-}" != "--fast" ]; then
      run_golden
    fi
    run_linux
    report
    ;;
esac
