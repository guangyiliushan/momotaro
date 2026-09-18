#!/usr/bin/env bash
# The guard in run_golden.sh stands between the repository and the only `rm -rf`
# in the tree, and a guard nobody executes is decoration. Every case runs the
# real script in a throwaway "repository" with `rm` stubbed out on PATH: the stub
# records that it was called, so a case passes only when the script refused *and*
# no deletion was even attempted. That also makes the absolute cases (`/`, `$HOME`)
# safe to try.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="$repo/tools/evals/run_golden.sh"
sandbox="${TMPDIR:-/tmp}/momotaro-guards-$$"
case "$sandbox" in
  /* | [A-Za-z]:[\\/]*) ;;
  *) sandbox="/tmp/momotaro-guards-$$" ;;
esac
rm -rf "$sandbox"
mkdir -p "$sandbox"

stub="$sandbox/bin"
mkdir -p "$stub"
cat > "$stub/rm" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${RM_CALLS:?}"
STUB
chmod +x "$stub/rm"

# A fake checkout the script must refuse to delete, carrying a copy of the script
# under test so `repo_posix` resolves to the fake.
fake="$sandbox/fake"
mkdir -p "$fake/tools/evals" "$fake/.git"
printf 'canary\n' > "$fake/tools/evals/canary"
printf 'x\n' > "$fake/Cargo.toml"
cp "$script" "$fake/tools/evals/"
mkdir -p "$sandbox/with-manifest"
printf 'x\n' > "$sandbox/with-manifest/Cargo.toml"
mkdir -p "$sandbox/parent-with-git/.git"
printf 'canary\n' > "$sandbox/parent-with-git/keep-me"

pass=0
fail=0

# refuses <label> <arg...>: the script must exit 2 without calling `rm`.
refuses() {
  local label="$1"
  shift
  : > "$sandbox/rm-calls"
  ( cd "$fake" && PATH="$stub:$PATH" RM_CALLS="$sandbox/rm-calls" \
      bash tools/evals/run_golden.sh "$@" ) >/dev/null 2>&1
  local code=$? called=0
  [ -s "$sandbox/rm-calls" ] && called=1
  if [ "$code" -eq 2 ] && [ "$called" -eq 0 ] && [ -e "$fake/tools/evals/canary" ]; then
    echo "ok   - refuses $label"
    pass=$((pass + 1))
  else
    echo "FAIL - refuses $label (exit $code, rm called=$called)"
    fail=$((fail + 1))
  fi
}

refuses "a relative path" "."
refuses "the repository itself" "$fake"
refuses "the repository's parent" "$sandbox"
refuses "a directory holding .git" "$sandbox/parent-with-git"
refuses "a git directory itself" "$sandbox/fake/.git"
refuses "a directory holding Cargo.toml" "$sandbox/with-manifest"
refuses "the home directory" "$HOME"
refuses "the filesystem root" "/"

# Positive control: a plain scratch directory is accepted, so the guard is not
# simply refusing everything. (`rm` is stubbed here too, and the scratch is
# inside the sandbox, so nothing real is built or deleted beyond the sandbox.)
: > "$sandbox/rm-calls"
( cd "$fake" && PATH="$stub:$PATH" RM_CALLS="$sandbox/rm-calls" \
    bash tools/evals/run_golden.sh "$sandbox/scratch" ) >/dev/null 2>&1
code=$?
if [ "$code" -eq 2 ]; then
  echo "FAIL - a plain scratch directory was refused (exit 2)"
  fail=$((fail + 1))
else
  echo "ok   - accepts a plain scratch directory (guard did not refuse; exit $code is later work)"
  pass=$((pass + 1))
fi

rm -rf "$sandbox"
echo "guards: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
