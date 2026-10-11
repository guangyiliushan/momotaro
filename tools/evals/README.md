# Retrieval evals (v0.2)

Golden-set regression for lexical search. This directory is intentionally
outside both the pnpm and Cargo workspaces (see docs/docs/python-rust.md).

## Files

- `gen_annotations.py` — generates `golden_set.json` (deterministic: fixed
  order, no timestamps). Edit this file and re-run it; commit both. Also owns
  the accepted-failure whitelist (`known_miss`) and the fixture hygiene check.
- `golden_set.json` — 15 main queries with `sufficient` sets (the minimal
  evidence set per the context-retrieval §3 "标充分集" caliber; three of them
  are multi-key so the AND semantics is actually exercised), per-query
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

Drop the gates to get the numbers without the D16 tripwires:

```bash
bash tools/evals/run_golden.sh /tmp/momotaro-eval      # scratch dir is optional
python tools/evals/eval_search.py --workspace /tmp/momotaro-eval --cli target/debug/momotaro-cli
```

`eval_search.py` resolves the CLI itself only via `cargo run`; outside a cargo
project pass `--cli` with the binary `cargo build -p momotaro-cli` reported.
A hard-coded `$PWD/target/debug/momotaro-cli` silently runs a **stale** one
whenever `CARGO_TARGET_DIR` (or a target triple) diverges from that path —
exactly the trap `run_golden.sh` was fixed to stop walking into.

## Recorded baseline (v0.2, cjk bigram, 2026-10-09)

Grown golden set: 15 main (12 + 3 multi-key) + 45 injection, 14-note corpus
(5 base vault + 9 eval fixtures):

```text
main  recall@5: 0.9000    mrr: 0.8222    misses: 0 unexpected, 2 displaced
inj   recall@5: 0.5000    mrr: 0.3667    misses: 12 fused, 12 displaced
hard-negative seats (main): 24  (semi_relevant 14, no_info 10)
poison seats (main): 8         (injection subset: 38)
```

Prior baselines, kept for the story: the first 5-file vault (12 queries,
2026-09) scored `recall@5 0.9583 / mrr 0.9028`; the expanded corpus
(2026-09-26) scored `0.9167 / 0.8333` on main and `0.5000 / 0.3519` on the
first injection subset, with 19 / 11 / 33 seats.

## What the misses are (and what the gates do about them)

The mean is no longer the gate. `--fail-on-unexpected-miss` fails on ANY
sub-1.0 row outside the whitelist, and the whitelist is capped per reason
(`--max-known-miss-fused`, `--max-known-miss-displaced`), so accepted failures
cannot quietly grow and a new failure cannot hide inside an average. MRR floors
sit below baseline by less than one minimal step (0.05/N); seat ceilings sit
exactly at baseline.

Two strata of accepted failure, both measured, both whitelisted with a `why`:

- **fused (12 rows)** — the query analyzer's own face: a whitespace-free base
  query (`傅里叶变换`, `谱定理`, `正交对角化`, `雅可比方法`) is fused by every
  injection shell into one phrase chunk no document contains; under the
  `system:` family only the shell word hits, the poison sample. This is a
  tokenizer property, not retrieval quality. If the analyzer ever tokenizes
  these, the rows pass and the whitelist entries simply stop applying. The
  split keeps this stratum from diluting the robustness story: of the rows the
  analyzer can actually parse, injection recall is `1.0` for 21/33 — the gap
  there is ranking, not tokenization. (Fixing the analyzer is a separate
  decision — the `cjk_tokenizer` config seam in context-retrieval §4.)
- **displaced (14 rows: 2 main + 12 injection)** — hits were retrieved but the
  sufficient source fell out of top-5:
  - `PCA 方差` (and its injection shells) — the recorded hard-negative harm:
    semi-pca-sklearn / fourier / long-mixed / the glossary / the poison sample
    fill all seats. This annotation is what makes the harm visible.
  - `SVD 协方差` (injection shells only) — shell text boosts the long notes'
    lexical overlap; pca.md drops out.
  - `RBF kernel 定义 调参` (injection shells only) — semi-kernel-svm.md is
    pushed out by shell noise; the base query is clean.
  - `特征值 定义 求法` — the new multi-key finding: sufficient member
    eigenvalue.md owns 4 of the 5 seats (four chunks) and fourier's aside
    takes the last, so long-mixed.md's 求法 never lands. One source's own
    chunks crowd out the other sufficient member. The rank-stage same-source
    demotion ("BM25 分数断崖的同文档段落降权", context-retrieval §3) is the
    designed fix; until then this row names the gap.

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
the `cjk_tokenizer` config seam) and the rank-stage demotion: both should
shrink the fused and displaced strata toward zero, at which point the ceilings
ratchet down in the same change.
