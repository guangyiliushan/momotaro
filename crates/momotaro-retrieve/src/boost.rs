//! Per-document origin-class weighting.
//!
//! Additive weighting (`bm25 + weight`) was rejected: the same constant moves
//! rank by a different amount for every query, while a multiplicative factor is
//! a scaling factor by construction — a document of the same class is always
//! scaled by the same ratio, whatever the query. The shape follows the
//! query-time composition tantivy's own primitives allow.
//!
//! tantivy 0.26.2 specifics: `BooleanQuery` hardcodes `SumCombiner`
//! (`boolean_query/boolean_query.rs:167`), so the class filter must contribute
//! `ConstScoreQuery::new(filter, 0.0)` — otherwise the filter clause itself
//! would add to the sum.

use tantivy::Term;
use tantivy::query::{BooleanQuery, BoostQuery, ConstScoreQuery, Query, TermQuery};
use tantivy::schema::IndexRecordOption;

use momotaro_contracts::OriginClass;

use crate::schema::Fields;

/// Every class a query may match, in branch order.
const RETRIEVABLE: [OriginClass; 3] = [OriginClass::Owner, OriginClass::Paper, OriginClass::Web];

/// Weight applied to one class; `None` means its text is never evidence.
///
/// Exhaustive on purpose: a new variant has to be classified here before the
/// crate compiles, and the wire spelling still comes from the contract
/// ([`OriginClass::as_str`]), never from a literal in this file.
const fn weight_of(class: OriginClass) -> Option<f32> {
    match class {
        OriginClass::Owner => Some(2.0),
        OriginClass::Paper => Some(1.0),
        OriginClass::Web => Some(0.6),
        OriginClass::Agent | OriginClass::System => None,
    }
}

/// Wraps `parsed` so that it can only match documents of `class`, and scales the
/// score of the documents it does match by `weight`.
fn class_branch(
    fields: &Fields,
    parsed: Box<dyn Query>,
    class: OriginClass,
    weight: f32,
) -> Box<dyn Query> {
    let filter = TermQuery::new(
        Term::from_field_text(fields.origin_class, class.as_str()),
        IndexRecordOption::Basic,
    );
    // `BooleanQuery` sums its clauses, so a clause that scored would add to the
    // hit's score: the filter contributes zero and nothing else.
    let filter: Box<dyn Query> = Box::new(ConstScoreQuery::new(Box::new(filter), 0.0));
    let both = BooleanQuery::intersection(vec![parsed, filter]);
    Box::new(BoostQuery::new(Box::new(both), weight))
}

/// Scopes `parsed` to the retrievable classes, each branch weighted.
///
/// A document carries exactly one class, so the branches are disjoint and a
/// document cannot be counted twice; if that ever stops being true (a
/// multi-valued class field), the union has to become a maximum instead.
pub(crate) fn origin_scoped(fields: &Fields, parsed: Box<dyn Query>) -> Box<dyn Query> {
    let branches = RETRIEVABLE
        .iter()
        .filter_map(|class| {
            weight_of(*class).map(|weight| class_branch(fields, parsed.box_clone(), *class, weight))
        })
        .collect();
    Box::new(BooleanQuery::union(branches))
}

#[cfg(test)]
mod tests {
    use momotaro_contracts::{Chunk, OriginClass};
    use tantivy::collector::TopDocs;
    use tantivy::query::QueryParser;
    use tantivy::schema::Value;
    use tantivy::{Index, TantivyDocument};

    use super::*;
    use crate::SearchIndex;
    use crate::cjk::{CJK_TOKENIZER_NAME, cjk_analyzer};
    use crate::query::{escape_query, normalize_query};
    use crate::schema;

    /// One chunk of `text` for `source_key`.
    fn chunk(source_key: &str, text: &str) -> Chunk {
        Chunk {
            chunk_id: format!("{source_key}#0"),
            source_key: source_key.to_string(),
            revision_hash: "rev-1".to_string(),
            ordinal: 0,
            heading_path: None,
            text: text.to_string(),
            locator_json: r#"{"start_byte":0}"#.to_string(),
            chunk_hash: format!("hash-{source_key}"),
            token_estimate: 10,
        }
    }

    /// A ram index holding one document per `(source_key, class, text)`.
    fn index_of(docs: &[(&str, OriginClass, &str)]) -> SearchIndex {
        let index = Index::create_in_ram(schema::build_schema());
        index
            .tokenizers()
            .register(CJK_TOKENIZER_NAME, cjk_analyzer());
        let fields = schema::fields(&index.schema()).expect("own schema resolves");
        let reader = index.reader().expect("reader");
        let search_index = SearchIndex::from_parts(index, fields, reader);
        let mut writer = search_index.writer(20_000_000).expect("writer");
        for (source_key, origin, text) in docs {
            SearchIndex::upsert_source(
                &mut writer,
                search_index.fields(),
                source_key,
                "T",
                *origin,
                &[chunk(source_key, text)],
            )
            .expect("upsert");
        }
        writer.commit().expect("commit");
        search_index
    }

    /// The score `search` reports for `source_key`.
    fn score_of(si: &SearchIndex, query: &str, source_key: &str) -> f64 {
        si.search(query, 10)
            .expect("search")
            .into_iter()
            .find(|hit| hit.source_key == source_key)
            .map(|hit| hit.score)
            .unwrap_or_else(|| panic!("no hit for {source_key}"))
    }

    /// The engine's own BM25 for `source_key`, with no class scoping — the
    /// baseline the weights are a ratio against.
    fn bare_bm25(si: &SearchIndex, query: &str, source_key: &str) -> f64 {
        let fields = si.fields();
        // The same fields production parses: a baseline that searched less would
        // not be a baseline.
        let parser = QueryParser::for_index(
            si.index(),
            vec![fields.text, fields.title, fields.heading_path],
        );
        let parsed = parser
            .parse_query(&escape_query(&normalize_query(query)))
            .expect("parse");
        let searcher = si.index().reader().expect("reader").searcher();
        let hits = searcher
            .search(&parsed, &TopDocs::with_limit(10).order_by_score())
            .expect("search");
        for (score, address) in hits {
            let doc: TantivyDocument = searcher.doc(address).expect("doc");
            if doc.get_first(fields.source_key).and_then(|v| v.as_str()) == Some(source_key) {
                return f64::from(score);
            }
        }
        panic!("no hit for {source_key}");
    }

    #[test]
    fn every_class_is_either_weighted_or_deliberately_excluded() {
        // The partition, not the numbers: tuning a weight is fine, but a class
        // must not fall out of retrieval (or into it) by accident.
        for class in RETRIEVABLE {
            assert!(
                weight_of(class).is_some(),
                "{class:?} is retrievable but has no weight"
            );
        }
        for class in [OriginClass::Agent, OriginClass::System] {
            assert_eq!(weight_of(class), None, "{class:?} must never be evidence");
        }
    }

    #[test]
    fn owner_chunk_outranks_equal_scoring_paper_chunk() {
        let si = index_of(&[
            ("note:owner.md", OriginClass::Owner, "alpha alpha"),
            ("note:paper.md", OriginClass::Paper, "alpha alpha"),
        ]);
        let hits = si.search("alpha", 10).expect("search");
        assert_eq!(hits[0].source_key, "note:owner.md", "{hits:?}");
    }

    #[test]
    fn boost_is_multiplicative_not_additive() {
        // The same text under 2.0 (owner) and 1.0 (paper). An additive boost
        // would make this ratio depend on the BM25 magnitude — it drifts with
        // query length and document statistics; a multiplicative one is exactly
        // the ratio of the weights, for every query.
        let si = index_of(&[
            ("note:owner.md", OriginClass::Owner, "alpha beta gamma"),
            ("note:paper.md", OriginClass::Paper, "alpha beta gamma"),
        ]);
        let owner = score_of(&si, "alpha", "note:owner.md");
        let paper = score_of(&si, "alpha", "note:paper.md");
        assert!(
            (owner / paper - 2.0).abs() < 1e-6,
            "owner={owner} paper={paper}"
        );
    }

    #[test]
    fn a_stronger_web_hit_still_loses_to_an_owner_note() {
        let si = index_of(&[
            ("note:owner.md", OriginClass::Owner, "alpha"),
            ("web:x", OriginClass::Web, &"alpha ".repeat(10)),
        ]);
        let bare_owner = bare_bm25(&si, "alpha", "note:owner.md");
        let bare_web = bare_bm25(&si, "alpha", "web:x");
        assert!(
            bare_web > bare_owner,
            "the web document really does score higher on its own: {bare_web} > {bare_owner}"
        );
        assert!(
            bare_web / bare_owner < 2.0 / 0.6,
            "…but not by enough for the class to stop mattering ({} vs {})",
            bare_web / bare_owner,
            2.0 / 0.6
        );
        let owner = score_of(&si, "alpha", "note:owner.md");
        let web = score_of(&si, "alpha", "web:x");
        assert!(owner > web, "owner={owner} web={web}");
    }

    #[test]
    fn agent_origin_never_appears() {
        // Query-side filtering: a document sitting in the index under a
        // non-retrievable class stays out of every result set.
        let si = index_of(&[
            ("note:mine.md", OriginClass::Owner, "alpha"),
            ("spark:1", OriginClass::Agent, "alpha"),
            ("sys:1", OriginClass::System, "alpha"),
        ]);
        assert_eq!(
            si.index().reader().expect("reader").searcher().num_docs(),
            3,
            "all three documents are in the index: the filter is what excludes them, not the fixture"
        );
        let hits = si.search("alpha", 10).expect("search");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].source_key, "note:mine.md");
    }

    #[test]
    fn the_class_filter_adds_no_score() {
        // `ConstScoreQuery(0.0)` is what keeps the filter clause out of the sum:
        // a single-class hit must be exactly the engine's BM25 times the weight.
        let si = index_of(&[("note:owner.md", OriginClass::Owner, "alpha beta")]);
        let scoped = score_of(&si, "alpha", "note:owner.md");
        let bare = bare_bm25(&si, "alpha", "note:owner.md");
        assert!(
            (scoped / bare - 2.0).abs() < 1e-6,
            "scoped={scoped} bare={bare}"
        );
    }

    #[test]
    fn ranks_are_dense_and_one_based_with_boosts() {
        let si = index_of(&[
            ("note:a.md", OriginClass::Owner, "alpha"),
            ("note:b.md", OriginClass::Paper, "alpha"),
            ("web:x", OriginClass::Web, "alpha"),
        ]);
        let hits = si.search("alpha", 10).expect("search");
        let ranks: Vec<u32> = hits.iter().map(|hit| hit.rank).collect();
        assert_eq!(ranks, (1..=3).collect::<Vec<u32>>(), "{ranks:?}");
    }

    #[test]
    fn a_query_that_matches_nothing_returns_no_hits() {
        // Only the scoped path is new here: the empty and punctuation queries are
        // covered by the crate's own garbage-query test.
        let si = index_of(&[("note:owner.md", OriginClass::Owner, "alpha")]);
        assert!(si.search("zulu", 10).expect("search").is_empty());
    }
}
