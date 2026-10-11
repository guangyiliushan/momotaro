//! Reciprocal-rank fusion for `--mode expand`.
//!
//! Fusing ranked lists is a pure function: same input lists, same output —
//! including the order, ties included. Nothing here reads a clock, a map with
//! a random seed, or the query text itself.

use std::collections::BTreeMap;

use momotaro_contracts::RetrievalHit;

/// The smoothing constant in `score = Σ 1 / (k + rank)`.
///
/// 60 is the conventional choice in the RRF literature; it is a property of the
/// ranking rule rather than a tuning surface, so the workspace has no
/// configuration for it.
pub const RRF_K: u32 = 60;

/// One accumulated hit: the row itself plus every list's contribution to it.
struct Fused {
    hit: RetrievalHit,
    /// `1 / (k + rank)` from each list the chunk appeared in, sorted before
    /// they are summed — see [`fuse_rrf`].
    contributions: Vec<f64>,
}

/// Fuse `k`-style reciprocal ranks over N ranked hit lists.
///
/// A chunk appearing in several lists scores the sum of `1 / (RRF_K + rank)` over
/// the lists that contain it, where `rank` is its **1-based position in that
/// list** (the position `SearchIndex::search` reports). The fused row keeps the
/// chunk's own fields — every list is a query over the same index, so all
/// occurrences describe the same chunk — and gets a new 1-based `rank` in the
/// fused order and `score` equal to the fused sum.
///
/// **Determinism.** Two rules, both load-bearing:
///
/// 1. Each chunk's contributions are sorted (`total_cmp`) before they are
///    summed, so the sum depends only on the *multiset* of contributions and
///    not on which list came first. Reordering the input lists therefore
///    produces bit-identical scores — floating-point addition is not
///    associative, and sorting is what removes that from the result.
/// 2. Ordering is `(score desc, source_key asc, ordinal asc, chunk_id asc)`.
///    The first three mirror the lexical path's documented tie-break; the last
///    is what makes the order a *total* one, so a full tie can never fall back
///    to whatever order the accumulator happened to produce.
///
/// Duplicate chunks inside one list are impossible (`SearchIndex::search`
/// returns each chunk at most once) but are tolerated: the contributions simply
/// accumulate.
///
/// # Examples
///
/// ```
/// use momotaro_contracts::RetrievalHit;
/// use momotaro_retrieve::fuse_rrf;
///
/// fn hit(chunk_id: &str) -> RetrievalHit {
///     RetrievalHit {
///         rank: 1,
///         source_key: format!("note:{chunk_id}.md"),
///         revision_hash: "rev-1".to_owned(),
///         chunk_id: chunk_id.to_owned(),
///         chunk_hash: format!("chash-{chunk_id}"),
///         ordinal: 0,
///         heading_path: None,
///         score: 0.0,
///         locator_json: r#"{"start_byte":0}"#.to_owned(),
///     }
/// }
///
/// // One ranked list per query; a chunk found by both lists scores both shares.
/// let fused = fuse_rrf(&[vec![hit("a"), hit("b")], vec![hit("b"), hit("a")]]);
/// let expected = 1.0 / 61.0 + 1.0 / 62.0;
/// assert_eq!(fused.len(), 2);
/// assert!((fused[0].score - expected).abs() < 1e-15);
/// assert!((fused[1].score - expected).abs() < 1e-15);
/// // The scores tie, so the documented tie-break decides: source_key first,
/// // never the order the lists were passed in.
/// assert_eq!(fused[0].chunk_id, "a");
/// assert_eq!(fused[0].rank, 1);
/// ```
pub fn fuse_rrf(lists: &[Vec<RetrievalHit>]) -> Vec<RetrievalHit> {
    // Keyed by chunk_id in a `BTreeMap` so accumulation itself has a defined
    // order too — nothing downstream may depend on hash iteration order.
    let mut fused: BTreeMap<String, Fused> = BTreeMap::new();

    for list in lists {
        for (position, hit) in list.iter().enumerate() {
            let rank = position as u32 + 1;
            let contribution = 1.0 / (f64::from(RRF_K) + f64::from(rank));
            fused
                .entry(hit.chunk_id.clone())
                .or_insert_with(|| Fused {
                    hit: hit.clone(),
                    contributions: Vec::with_capacity(lists.len()),
                })
                .contributions
                .push(contribution);
        }
    }

    let mut rows: Vec<(String, Fused)> = fused.into_iter().collect();
    for (_, entry) in &mut rows {
        entry.contributions.sort_by(f64::total_cmp);
        entry.hit.score = entry.contributions.iter().sum();
    }

    rows.sort_by(|(left_id, left), (right_id, right)| {
        right
            .hit
            .score
            .total_cmp(&left.hit.score)
            .then_with(|| left.hit.source_key.cmp(&right.hit.source_key))
            .then_with(|| left.hit.ordinal.cmp(&right.hit.ordinal))
            .then_with(|| left_id.cmp(right_id))
    });

    rows.into_iter()
        .enumerate()
        .map(|(position, (_, mut entry))| {
            entry.hit.rank = position as u32 + 1;
            entry.hit
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(chunk_id: &str, source_key: &str, ordinal: u32) -> RetrievalHit {
        RetrievalHit {
            rank: 1,
            source_key: source_key.to_owned(),
            revision_hash: format!("rev-{source_key}"),
            chunk_id: chunk_id.to_owned(),
            chunk_hash: format!("hash-{chunk_id}"),
            ordinal,
            heading_path: None,
            score: 0.0,
            locator_json: "{}".to_owned(),
        }
    }

    /// `["a", "b"]` style helper: chunk ids in rank order for one list.
    fn list(spec: &[(&str, &str, u32)]) -> Vec<RetrievalHit> {
        spec.iter()
            .enumerate()
            .map(|(position, (chunk, source, ordinal))| {
                let mut row = hit(chunk, source, *ordinal);
                row.rank = position as u32 + 1;
                row
            })
            .collect()
    }

    fn ids(hits: &[RetrievalHit]) -> Vec<&str> {
        hits.iter().map(|row| row.chunk_id.as_str()).collect()
    }

    #[test]
    fn a_chunk_found_by_several_lists_scores_the_sum_of_its_contributions() {
        // x ranks 1st in list A and 2nd in list B: 1/(60+1) + 1/(60+2).
        let lists = [
            list(&[("x", "s1", 0), ("y", "s2", 0)]),
            list(&[("y", "s2", 0), ("x", "s1", 0)]),
        ];
        let fused = fuse_rrf(&lists);

        let x = fused.iter().find(|row| row.chunk_id == "x").expect("x");
        let y = fused.iter().find(|row| row.chunk_id == "y").expect("y");
        let expected_x = 1.0 / 61.0 + 1.0 / 62.0;
        let expected_y = 1.0 / 62.0 + 1.0 / 61.0;
        assert!(
            (x.score - expected_x).abs() < 1e-15,
            "x scores 1/(61)+1/(62): {} vs {expected_x}",
            x.score
        );
        assert!(
            (y.score - expected_y).abs() < 1e-15,
            "{} vs {expected_y}",
            y.score
        );
        // Both lists contain both chunks, so the sums tie and only the
        // documented tie-break separates them.
        assert_eq!(ids(&fused), ["x", "y"], "source_key ascending on a tie");
    }

    #[test]
    fn presence_in_more_lists_beats_a_better_rank_in_fewer() {
        // z is 1st in one list; w is 3rd in two lists.
        let lists = [
            list(&[("z", "s1", 0), ("w", "s2", 0), ("q", "s3", 0)]),
            list(&[("a", "s4", 0), ("b", "s5", 0), ("w", "s2", 0)]),
        ];
        let fused = fuse_rrf(&lists);
        assert_eq!(ids(&fused)[0], "w", "two appearances outrank one top rank");
        assert_eq!(fused[0].rank, 1, "the fused rank is 1-based");
    }

    #[test]
    fn reordering_the_input_lists_does_not_move_anything() {
        // Reordering the input lists must not reorder the answer. This asserts
        // the *order*; the score half of the claim — that arrival order cannot
        // leak into `score` at all — is the one with teeth, in
        // `the_fused_score_is_the_sum_of_the_sorted_contributions`, whose
        // fixture (ranks 1/2/8) is chosen because those two summation orders
        // are one ULP apart.
        let a = list(&[("x", "s1", 0), ("y", "s2", 0), ("z", "s3", 0)]);
        let b = list(&[("y", "s2", 0), ("z", "s3", 0), ("x", "s1", 0)]);
        let c = list(&[("z", "s3", 0), ("x", "s1", 0)]);

        let forward = fuse_rrf(&[a.clone(), b.clone(), c.clone()]);
        let backward = fuse_rrf(&[c, b, a]);

        assert_eq!(ids(&forward), ids(&backward), "same order either way");
        for (left, right) in forward.iter().zip(&backward) {
            assert_eq!(
                left.score.to_bits(),
                right.score.to_bits(),
                "{}: the fused score must be bit-identical, not merely close",
                left.chunk_id
            );
        }
        // x and z really are tied on score — which is why the tie-break, not
        // the summation order, is what separates them.
        assert_eq!(ids(&forward), ["x", "z", "y"]);
        assert_eq!(
            forward[0].score.to_bits(),
            forward[1].score.to_bits(),
            "x and z arrive at the same score"
        );
    }

    #[test]
    fn the_order_is_total_even_for_a_complete_tie() {
        // Each chunk is rank 1 in exactly one list: identical scores,
        // identical source_key, identical ordinal. `score`, `source_key` and
        // `ordinal` are all tied, so only `chunk_id` can decide — and the
        // answer must not depend on which list happened to be visited first.
        let tied = [
            list(&[("c", "s", 0)]),
            list(&[("a", "s", 0)]),
            list(&[("b", "s", 0)]),
        ];
        let forward = fuse_rrf(&tied);
        let backward = fuse_rrf(&[tied[2].clone(), tied[1].clone(), tied[0].clone()]);

        let scores: Vec<u64> = forward.iter().map(|row| row.score.to_bits()).collect();
        assert!(
            scores.windows(2).all(|pair| pair[0] == pair[1]),
            "a genuine three-way tie: {scores:?}"
        );
        assert_eq!(ids(&forward), ["a", "b", "c"]);
        assert_eq!(ids(&backward), ids(&forward), "the tie-break is stable");
    }

    /// A list in which `x` sits at the given 1-based rank; every other entry
    /// exists only to hold that position, and is unique to the list so it never
    /// collects a second contribution.
    fn list_with_x_at(rank: u32) -> Vec<RetrievalHit> {
        let mut list = Vec::new();
        while list.len() as u32 + 1 < rank {
            let filler = list.len();
            list.push(hit(
                &format!("pad{rank}-{filler}"),
                &format!("s{rank}-{filler}"),
                filler as u32,
            ));
        }
        list.push(hit("x", "sx", 0));
        list
    }

    #[test]
    fn the_fused_score_is_the_sum_of_the_sorted_contributions() {
        // Ranks 1, 2 and 8 — chosen because their three orders are NOT the same
        // number: `((1/61 + 1/62) + 1/68)` differs from
        // `((1/68 + 1/62) + 1/61)` by one ULP in IEEE-754. So the sort inside
        // `fuse_rrf` is load-bearing, not decorative: with it removed the two
        // runs below stop agreeing with each other (and with `expected`).
        let forward = fuse_rrf(&[list_with_x_at(1), list_with_x_at(2), list_with_x_at(8)]);
        let backward = fuse_rrf(&[list_with_x_at(8), list_with_x_at(2), list_with_x_at(1)]);

        // Ascending contribution order, evaluated left to right — which is
        // exactly what the sorted sum produces, whichever list came first.
        let expected: f64 = 1.0 / 68.0 + 1.0 / 62.0 + 1.0 / 61.0;
        for (name, fused) in [("forward", &forward), ("backward", &backward)] {
            let x = fused.iter().find(|row| row.chunk_id == "x").expect("x");
            assert_eq!(
                x.score.to_bits(),
                expected.to_bits(),
                "{name}: the contributions must be summed in a defined order, not arrival order"
            );
        }
    }

    #[test]
    fn the_sort_keys_are_total_on_their_own() {
        // Structural, because the accumulator is a `BTreeMap` keyed by
        // `chunk_id`: its iteration order happens to be the same rule as the
        // fourth sort key, so *no behavioural tie test can tell the comparator
        // from the container*. Each key is read by its exact expression rather
        // than counted — counting `then_with(` would accept the three arrows in
        // any order or any direction.
        let source = include_str!("fusion.rs");
        let body = source
            .split("rows.sort_by(")
            .nth(1)
            .expect("the fused rows are sorted")
            .split("\n    });")
            .next()
            .expect("the sort_by body");

        // Position, not presence: the fusion rule specifies an *order*, and `contains`
        // would accept the four keys in any sequence.
        let keys = [
            ".total_cmp(&left.hit.score)",
            "left.hit.source_key.cmp(&right.hit.source_key)",
            "left.hit.ordinal.cmp(&right.hit.ordinal)",
            "left_id.cmp(right_id)",
        ];
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| {
                body.find(key)
                    .unwrap_or_else(|| panic!("the comparator must carry `{key}`: {body}"))
            })
            .collect();
        for pair in positions.windows(2) {
            assert!(
                pair[0] < pair[1],
                "score, then source_key, then ordinal, then chunk_id — in that order: {body}"
            );
        }
        assert_eq!(
            body.matches("then_with(").count(),
            3,
            "exactly three `then_with` after the score comparison: {body}"
        );
    }

    #[test]
    fn source_key_is_consulted_before_ordinal() {
        // Both chunks are rank 1 in their own list, so the scores tie exactly,
        // and they differ in *both* of the next dimensions: only a comparator
        // that asks `source_key` before `ordinal` can answer this. Every other
        // tie fixture holds one of the two constant, so none of them would.
        let fused = fuse_rrf(&[list(&[("cA", "s1", 9)]), list(&[("cB", "s2", 0)])]);

        let scores: Vec<u64> = fused.iter().map(|row| row.score.to_bits()).collect();
        assert!(
            scores.windows(2).all(|pair| pair[0] == pair[1]),
            "a genuine tie: {scores:?}"
        );
        assert_eq!(
            ids(&fused),
            ["cA", "cB"],
            "source_key ascending decides, not ordinal"
        );
    }

    #[test]
    fn ordinal_decides_when_score_and_source_key_tie() {
        // Two chunks of one source, each rank 1 in a list of its own: the same
        // score, the same source_key, so only `ordinal` separates them. With
        // that key reversed the order flips — and nothing else in this module
        // would notice, because every other fixture uses ordinal 0.
        let fused = fuse_rrf(&[list(&[("cA", "s", 0)]), list(&[("cB", "s", 5)])]);

        let scores: Vec<u64> = fused.iter().map(|row| row.score.to_bits()).collect();
        assert!(
            scores.windows(2).all(|pair| pair[0] == pair[1]),
            "a genuine score tie: {scores:?}"
        );
        assert_eq!(
            ids(&fused),
            ["cA", "cB"],
            "ascending ordinal breaks the tie"
        );
    }

    #[test]
    fn the_fused_row_keeps_the_chunk_it_came_from() {
        let mut source = hit("x", "s1", 3);
        source.heading_path = Some("谱定理 > 定义".to_owned());
        source.locator_json = r#"{"start_byte":10,"start_line":2}"#.to_owned();
        let lists = [vec![source.clone()]];

        let fused = fuse_rrf(&lists);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].source_key, source.source_key);
        assert_eq!(fused[0].revision_hash, source.revision_hash);
        assert_eq!(fused[0].chunk_hash, source.chunk_hash);
        assert_eq!(fused[0].ordinal, source.ordinal);
        assert_eq!(fused[0].heading_path, source.heading_path);
        assert_eq!(fused[0].locator_json, source.locator_json);
        assert_eq!(fused[0].rank, 1);
        // The score is the fused sum, never the per-query BM25 it arrived with.
        assert!((fused[0].score - 1.0 / 61.0).abs() < 1e-15);
    }

    #[test]
    fn no_lists_and_no_hits_both_yield_nothing() {
        assert!(fuse_rrf(&[]).is_empty());
        assert!(fuse_rrf(&[Vec::new(), Vec::new()]).is_empty());
    }

    #[test]
    fn the_smoothing_constant_is_sixty() {
        // Pinned rather than implied: changing `RRF_K` changes every ranking,
        // and a "conventional value" that silently drifts is not a convention.
        assert_eq!(RRF_K, 60);
    }
}
