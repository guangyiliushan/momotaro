# Retrieval evals (v0.2)

Golden-set regression for lexical search. This directory is intentionally
outside both the pnpm and Cargo workspaces (see docs/docs/python-rust.md).

## Files

- `golden_set.json` — 12 queries (8 zh / 4 en) with expected `source_key`s
  from the fixture vault at `crates/momotaro-run/tests/fixtures/vault/`.
- `eval_search.py` — shells the CLI, computes recall@k and MRR, prints a
  table. Standard library only.

## Run

```bash
# Builds the CLI, indexes the fixture vault in a scratch directory (never in the
# repo root: `init` writes `.momotaro/` where it runs) and gates the golden set
# with the same thresholds CI uses.
bash tools/evals/run_golden.sh
```

Drop the floors to get the numbers without the D16 gate:

```bash
bash tools/evals/run_golden.sh /tmp/momotaro-eval      # scratch dir is optional
python tools/evals/eval_search.py --workspace /tmp/momotaro-eval \
    --cli "$PWD/target/debug/momotaro-cli" --k 5       # advisory, no floor
```

The first full run established the BM25-bigram baseline. Any future tokenizer
or ranking change must match or beat that number before merging (D16).

## Recorded baseline (v0.2, cjk bigram)

Fixture vault, k=5:

```text
recall@5: 0.9583
mrr:      0.9028
```

CI gates on the thresholds inside `run_golden.sh` — the instrument is
deterministic, so they are tripwires, not tolerances, and a bigger golden set
must reset them in the same change.

Known miss: `特征值 特征向量` — the multi-word zh query ranks fourier.md and
long-mixed.md into all top-5 slots, pushing pca.md out. Single-word zh/en
queries all rank rank-1. This is the reference behavior for judging future
tokenizer swaps (e.g. jieba via the `cjk_tokenizer` config seam).
