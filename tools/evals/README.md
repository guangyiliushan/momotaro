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
# 1. build a workspace against the fixture vault
cargo run -p momotaro-cli -- init
cargo run -p momotaro-cli -- index ./crates/momotaro-run/tests/fixtures/vault

# 2. evaluate (advisory — no floor)
python tools/evals/eval_search.py --workspace .

# 3. gate (fails below the floor — the D16 upgrade gate)
python tools/evals/eval_search.py --workspace . --k 5 --floor-recall 0.9
```

The first full run establishes the BM25-bigram baseline. Any future tokenizer
or ranking change must match or beat that number before merging (D16).

## Recorded baseline (v0.2, cjk bigram, 2026-09-14)

Fixture vault, k=5, CLI build at this commit:

```text
recall@5: 0.9583
mrr:      0.9028
```

Known miss: `特征值 特征向量` — the multi-word zh query ranks fourier.md and
long-mixed.md into all top-5 slots, pushing pca.md out. Single-word zh/en
queries all rank rank-1. This is the reference behavior for judging future
tokenizer swaps (e.g. jieba via the `cjk_tokenizer` config seam).
