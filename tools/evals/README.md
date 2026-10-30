# Retrieval evals (v0.2)

Golden-set regression for lexical search. This directory is intentionally
outside both the pnpm and Cargo workspaces.

## Files

- `gen_annotations.py` — generates `golden_set.json` (deterministic: fixed
  order, no timestamps). Edit this file and re-run it; commit both. Also owns
  the accepted-failure whitelist (`known_miss`) and the fixture hygiene check.
- `golden_set.json` — 15 main queries with `sufficient` sets (per-query
  "标充分集", the minimal evidence set; three of them are multi-key so the
  AND semantics is actually exercised), per-query
  `hard_negatives` labels (`semi_relevant` / `no_info`, each with a `why`), a
  45-entry `injection` regression subset (prompt-injection text embedded in
  the query; retrieval surface only), and `poison_keys` naming the corpus-side
  attack samples. Rows scoring below 1.0 must carry `known_miss` (`fused` |
  `displaced`, with a `why`) — the whitelist is in the data, not in an
  average.
- `fixtures/` — eval-owned vault notes copied into the scratch vault by
  `run_golden.sh`: three semi-relevant notes, one no-info glossary, two poison
  samples, one bad-frontmatter note (`reason_code =
  frontmatter_parse_error` is asserted on the ingest audit row), plus two
  dedicated answer notes (`eigenvalue.md`, `kernel-def.md`) that re-anchor the
  `特征值 特征向量` / `kernel` queries onto notes that actually answer them.
- `eval_search.py` — shells the CLI, computes report A (utility: recall@k,
  MRR, per set) and report B (robustness: top-k seats owned by hard negatives
  and poison samples), prints both. Gates are per-row: any sub-1.0 row outside
  the whitelist fails, and the whitelist's size is capped per reason. Standard
  library only.
- `test_eval_search.py` / `test_guards.sh` — the harness's own self-tests
  (malformed-input boundary, miss accounting, gate wiring; `rm -rf` guard).

## Run

```bash
# Builds the CLI, indexes the fixture vault plus tools/evals/fixtures in a
# scratch directory (never in the repo root: `init` writes `.momotaro/` where
# it runs) and gates the golden set with the same thresholds CI uses.
bash tools/evals/run_golden.sh
```

Drop the gates to get the numbers without the miss tripwires:

```bash
bash tools/evals/run_golden.sh /tmp/momotaro-eval      # scratch dir is optional
python tools/evals/eval_search.py --workspace /tmp/momotaro-eval --cli target/debug/momotaro-cli
```

`eval_search.py` resolves the CLI itself only via `cargo run`; outside a cargo
project pass `--cli` with the binary `cargo build -p momotaro-cli` reported.
A hard-coded `$PWD/target/debug/momotaro-cli` silently runs a **stale** one
whenever `CARGO_TARGET_DIR` (or a target triple) diverges from that path —
exactly the trap `run_golden.sh` was fixed to stop walking into.

## Recorded baseline (v0.5, cjk bigram, 2026-10-30)

15 main (12 + 3 multi-key) + 45 injection, 14-note corpus (5 base vault + 9
eval fixtures). Three changes stand under these numbers: the query is a bag of
analyzer terms; the rank stage takes each source's best chunk before any
source's second; and the notes use the vocabulary the questions are asked with.

```text
main  recall@5: 1.0000    mrr: 0.8778    misses: 0 unexpected, 0 displaced
inj   recall@5: 0.9444    mrr: 0.4104    misses: 0 fused, 4 displaced
hard-negative seats (main): 31  (semi_relevant 17, no_info 14)
poison seats (main): 14         (injection subset: 90)
```

The third of those was the one that mattered. Three annotations were
unreachable rather than outranked: `pca.md` answered `PCA 方差` while never
writing 方差 (it said "covariance matrix"), and the eigenvalue notes discussed
the definition and the method without using those words. Retrieval matches on
terms, so no ranking rule could have put them first — and the miss rows read as
a ranking problem for as long as nothing measured reachability. The notes now
use the query's vocabulary, and `gen_annotations.py` refuses an annotation
whose evidence never carries the query's terms. Main is at recall 1.0.

What is left in injection is the attack surface itself: two poison samples
carry the shell text verbatim and take two of the five seats, and the sufficient
source is sixth behind `only-code` and `semi-pca-sklearn`. Retrieval retrieving
is not the injection defence; the untrusted-content delimiters and the prompt
hierarchy are, and this set is the retrieval surface only.

The floors sit within one minimal step (`0.05/N`) of the readings (`0.877` /
`0.410`) and the seat ceilings exactly at them, so losing the gain trips. The
known-miss ceilings ratcheted down in the same changes: `fused` stays **0**,
`displaced` 12 → 9 → 8 → **4**.

Prior baselines, kept for the story: the first 5-file vault (12 queries,
2026-09) scored `recall@5 0.9583 / mrr 0.9028`; the expanded corpus (2026-09-26)
scored `0.9167 / 0.8333` on main and `0.5000 / 0.3519` on the first injection
subset, with 19 / 11 / 33 seats; the grown set at 2026-10-09 scored
`0.9000 / 0.8222` on main and `0.5000 / 0.3667` on injection, with 24 / 8 / 38;
after the query fix (2026-10-29) main `0.9000 / 0.8556` and injection
`0.8667 / 0.4107`, with 25 / 8 / 87; after the rank stage (2026-10-30) main
`0.9333 / 0.8556` and injection `0.8889 / 0.4107`, with 31 / 15 / 90.

## What the misses are (and what the gates do about them)

The mean is no longer the gate. `--fail-on-unexpected-miss` fails on ANY
sub-1.0 row outside the whitelist, and the whitelist is capped per reason
(`--max-known-miss-fused`, `--max-known-miss-displaced`), so accepted failures
cannot quietly grow and a new failure cannot hide inside an average. MRR floors
sit below baseline by less than one minimal step (0.05/N); seat ceilings sit
exactly at baseline.

Two strata of accepted failure, both measured, both whitelisted with a `why`:

- **fused (0 rows)** — gone, and it was never the tokenizer's face. What looked
  like the analyzer refusing to split a whitespace-free base query was
  `QueryParser` turning the shell-plus-first-word chunk into a `PhraseQuery` no
  document holds; the search then answered on whatever followed the last space.
  Terms now come from the analyzer one at a time, so the rows pass, and the
  whitelist no longer names them. The ceiling is 0, so any phrase-shaped query
  trips it as an unexpected miss rather than being absorbed.
- **displaced (4 rows, injection only)** — hits were retrieved but the
  sufficient source fell out of top-5. Every window holds five distinct sources
  and both poison samples are in two of them:
  - `RBF kernel 定义 调参` (all three shells) — semi-kernel-svm.md is sixth
    behind poison-ignore / poison-system / only-code / semi-pca-sklearn /
    kernel-def. The base query is clean, and it was clean before the shells too.
  - `PCA 方差` (the `system:` shell only) — pca.md is sixth behind the two
    poison samples, semi-pca-sklearn, fourier and long-mixed. It passes on the
    main set and on the other two shells.

  Three rows used to sit here for a different reason entirely: the annotated
  sufficient source never contained the words the question was asked with.
  `pca.md` wrote "covariance matrix" where the query said 方差; the eigenvalue
  notes discussed the definition and the method without those words. Retrieval
  matches on terms, so those rows could not have been won by ranking — and they
  read as ranking failures to everyone who looked, this README's own earlier
  diagnosis included. The notes use the query's vocabulary now, and
  `reachability_hygiene` in `gen_annotations.py` fails the build over an
  annotation whose evidence never carries a query term.

  `SVD 协方差` and `特征值 定义 求法` left with them.

## Blind spots, kept deliberately

- The seat ceilings catch offenders taking MORE top-k seats, but a bug that
  undercounts seats would sail under them. The semi_relevant / no_info split
  (14/10) is recorded in the report but not gated — a swap passes. Both are
  acceptable: report B is a harm gauge on top of report A's utility gates, not
  an independent identity check.
- `known_miss` rows' depth of failure is not pinned (a whitelisted 0.5 row may
  sink to 0.0 without tripping); the reason-count ceilings bound how many such
  rows exist, not how bad they get.
- Single-source recall is still 0/1 per member; the multi-key sets fix the AND
  semantics but chunk-level ("the right chunk in context") is not annotated —
  `特征值 定义 求法` is the loud reminder.

This is the reference behavior for judging future tokenizer swaps (jieba via
the `cjk_tokenizer` config seam) and any further rank-stage work. The ratchet
has turned three times: `fused` went to zero with its ceiling, and `displaced`
came 12 → 9 → 8 → 4 as the query fix, the rank stage and then the fixture
vocabulary landed. Whatever shrinks it next brings the ceiling down in that
same change — not in a later tidy-up.
