# Retrieval evals (v0.2)

Golden-set regression for lexical search. This directory is intentionally
outside both the pnpm and Cargo workspaces (see docs/docs/python-rust.md).

## Files

- `gen_annotations.py` — generates `golden_set.json` (deterministic: fixed
  order, no timestamps). Edit this file and re-run it; commit both.
- `golden_set.json` — 12 main queries with `sufficient` sets (the minimal
  evidence set per the context-retrieval §3 "标充分集" caliber), per-query
  `hard_negatives` labels (`semi_relevant` / `no_info`, each with a `why`), a
  36-entry `injection` regression subset (prompt-injection text embedded in
  the query; retrieval surface only), and `poison_keys` naming the corpus-side
  attack samples.
- `fixtures/` — eval-owned vault notes copied into the scratch vault by
  `run_golden.sh`: three semi-relevant notes, one no-info glossary, two poison
  samples, and one bad-frontmatter note (the W2.3 handover: `reason_code =
  frontmatter_parse_error` is asserted on the ingest audit row).
- `eval_search.py` — shells the CLI, computes report A (utility: recall@k,
  MRR, per set) and report B (robustness: top-k seats owned by hard negatives
  and poison samples), prints both. Standard library only.

## Run

```bash
# Builds the CLI, indexes the fixture vault plus tools/evals/fixtures in a
# scratch directory (never in the repo root: `init` writes `.momotaro/` where
# it runs) and gates the golden set with the same thresholds CI uses.
bash tools/evals/run_golden.sh
```

Drop the floors to get the numbers without the D16 gate:

```bash
bash tools/evals/run_golden.sh /tmp/momotaro-eval      # scratch dir is optional
python tools/evals/eval_search.py --workspace /tmp/momotaro-eval \
    --cli "$PWD/target/debug/momotaro-cli" --k 5       # advisory, no floor
```

## Recorded baseline (v0.2, cjk bigram)

First baseline (5-file fixture vault, 12 queries, recorded 2026-09):

```text
recall@5: 0.9583
mrr:      0.9028
```

Expanded baseline (fixture vault + tools/evals/fixtures, 12 main + 36
injection queries, recorded 2026-09-26). Report A / report B:

```text
main  recall@5: 0.9167    mrr: 0.8333
inj   recall@5: 0.5000    mrr: 0.3519
hard-negative seats (main): 19  (semi_relevant 9, no_info 10)
poison seats (main): 11        (injection subset: 33)
```

The main-set drop from 0.9583 is the documented hard-negative harm, not a
retrieval change: the semi-relevant notes and the no-info glossary compete for
top-5 seats (`PCA 方差` loses pca.md from top-5 entirely). The injection
subset's 18 zero-recall rows come in two shapes, both measured. 12 rows leave
the base term fused into a whitespace-free phrase chunk that no document
contains: 8 queries have no whitespace anywhere and return zero hits; in the
`system:` family the shell's own space splits off a shell word that matches
the poison sample, so those return exactly its hit. A space anywhere splits
the query into chunks the analyzer matches independently, and retrieval
recovers. The other 6 rows retrieve hits but their sufficient source is
displaced from top-5 by the same hard-negative competition as the main set.
This is the retrieval face of the query analyzer; no LLM is involved anywhere
in the harness.

CI gates on the thresholds inside `run_golden.sh` — the instrument is
deterministic (the reported metrics are bit-identical across runs; the full
script stdout is not, it carries timing lines), so the thresholds are
tripwires, not tolerances. Each floor sits below its baseline by less than one
minimal step and each ceiling sits exactly at it, so any real regression trips.
When the golden set grows again, re-measure and reset the thresholds in the
same change.

One blind spot, kept deliberately: the seat ceilings catch offenders taking
MORE top-k seats, but a hypothetical bug that undercounts seats would sail
under them (the ceilings say "at most N", not "exactly N"). The semi_relevant
/ no_info split (9/10) is recorded in the report but not gated — a 9/10 to
10/9 swap passes. Both are acceptable: report B is a harm gauge on top of
report A's utility floors, not an independent identity check.

Known miss (current): `PCA 方差` — on the expanded corpus the semi-relevant
sklearn note, fourier.md's PCA aside, long-mixed.md's geometric aside, the
glossary and the poison sample fill all top-5 seats, pushing pca.md out. This
is exactly the harm the hard-negative annotations exist to make visible. (On
the 5-file vault the known miss was `特征值 特征向量` — pca.md crowded out by
fourier.md/long-mixed.md chunks; under the minimal-sufficient-set caliber that
query is answered by long-mixed.md alone and no longer misses.) Single-word
zh/en queries on the 5-file vault all rank at rank 1. This is the reference
behavior for judging future tokenizer swaps (e.g. jieba via the
`cjk_tokenizer` config seam).
