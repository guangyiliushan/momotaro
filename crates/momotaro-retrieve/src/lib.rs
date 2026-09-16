//! Tantivy-backed lexical retrieval.
//!
//! Owns the index schema, the CJK bigram tokenizer, and query execution.
//! Depends only on contracts and tantivy — never on the store, which stays
//! the canonical home of chunk text.

#![forbid(unsafe_code)]

pub mod cjk;
pub mod index;
pub mod query;
pub mod schema;

pub use cjk::{CJK_TOKENIZER_NAME, CjkTokenizer, cjk_analyzer};
pub use index::{IndexWriterHandle, SearchIndex};
pub use query::{escape_query, normalize_query};
pub use schema::{Fields, build_schema, doc_from_chunk, fields};

use tantivy::TantivyError;
use thiserror::Error;

/// Errors surfaced by the retrieval engine.
#[derive(Debug, Error)]
pub enum RetrieveError {
    /// Underlying tantivy failure.
    #[error(transparent)]
    Tantivy(#[from] TantivyError),
    /// User query could not be parsed even after escaping.
    #[error("query parse failed: {0}")]
    QueryParse(String),
    /// Expected an existing index directory but found none.
    #[error("index directory not found: {0}")]
    IndexMissing(String),
    /// The on-disk index schema was not produced by this build (stale,
    /// truncated, or foreign index). Rebuild the index to recover.
    #[error("index schema mismatch: missing field `{0}`; rebuild the index")]
    SchemaMismatch(String),
    /// Filesystem failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use momotaro_contracts::Chunk;

    use super::*;

    fn chunk(
        chunk_id: &str,
        source_key: &str,
        ordinal: u32,
        heading: Option<&str>,
        text: &str,
    ) -> Chunk {
        Chunk {
            chunk_id: chunk_id.to_string(),
            source_key: source_key.to_string(),
            revision_hash: "rev-1".to_string(),
            ordinal,
            heading_path: heading.map(str::to_string),
            text: text.to_string(),
            locator_json: r#"{"start_byte":0}"#.to_string(),
            chunk_hash: format!("hash-{chunk_id}"),
            token_estimate: 10,
        }
    }

    fn ram_index() -> SearchIndex {
        let index = tantivy::Index::create_in_ram(schema::build_schema());
        index
            .tokenizers()
            .register(CJK_TOKENIZER_NAME, cjk_analyzer());
        let fields = schema::fields(&index.schema()).expect("own schema resolves");
        let reader = index.reader().expect("reader");
        SearchIndex::from_parts(index, fields, reader)
    }

    #[test]
    fn zh_query_ranks_zh_chunk_first() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        let chunks = [
            chunk(
                "zh-1",
                "ml/fourier.md",
                0,
                Some("傅里叶"),
                "傅里叶变换将信号从时域映射到频域。",
            ),
            chunk(
                "en-1",
                "ml/basics.md",
                0,
                None,
                "The Fourier transform maps signals between domains.",
            ),
            chunk("zh-2", "ml/prob.md", 1, None, "概率论是统计学的基石。"),
        ];
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "ml/fourier.md",
            "傅里叶笔记",
            &chunks[..1],
        )
        .expect("upsert zh");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "ml/basics.md",
            "Basics",
            &chunks[1..2],
        )
        .expect("upsert en");
        SearchIndex::upsert_source(&mut writer, si.fields(), "ml/prob.md", "概率", &chunks[2..])
            .expect("upsert zh2");
        writer.commit().expect("commit");

        let hits = si.search("傅里叶", 10).expect("search");
        assert!(!hits.is_empty());
        assert_eq!(hits[0].chunk_id, "zh-1");
        assert_eq!(hits[0].rank, 1);
        assert_eq!(hits[0].source_key, "ml/fourier.md");
        assert_eq!(hits[0].revision_hash, "rev-1");
        assert_eq!(hits[0].chunk_hash, "hash-zh-1");
        assert_eq!(hits[0].ordinal, 0);
        assert_eq!(hits[0].heading_path.as_deref(), Some("傅里叶"));
        assert_eq!(hits[0].locator_json, r#"{"start_byte":0}"#);
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn english_query_hits_english_chunk() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "ml/basics.md",
            "Basics",
            &[chunk(
                "en-1",
                "ml/basics.md",
                0,
                None,
                "gradient descent optimization",
            )],
        )
        .expect("upsert");
        writer.commit().expect("commit");

        let hits = si.search("gradient descent", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, "en-1");
    }

    #[test]
    fn garbage_and_empty_queries_return_ok_empty() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "a.md",
            "A",
            &[chunk("a-1", "a.md", 0, None, "some content here")],
        )
        .expect("upsert");
        writer.commit().expect("commit");

        assert!(si.search("", 10).expect("empty").is_empty());
        assert!(si.search("   ", 10).expect("spaces").is_empty());
        assert!(si.search("!!!", 10).expect("punct").is_empty());
        assert!(si.search("  !!! ??? ***  ", 10).expect("mixed").is_empty());
    }

    #[test]
    fn upsert_replaces_chunk_set() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "a.md",
            "A",
            &[
                chunk("a-old-1", "a.md", 0, None, "傅里叶变换 old first"),
                chunk("a-old-2", "a.md", 1, None, "second chunk about 变换"),
            ],
        )
        .expect("upsert v1");
        writer.commit().expect("commit v1");
        assert_eq!(si.search("傅里叶", 10).expect("hits").len(), 1);

        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "a.md",
            "A",
            &[chunk("a-new-1", "a.md", 0, None, "傅里叶变换 new content")],
        )
        .expect("upsert v2");
        writer.commit().expect("commit v2");

        let hits = si.search("傅里叶", 10).expect("hits v2");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, "a-new-1");
        let all = si.search("chunk", 10).expect("all");
        assert!(
            all.iter()
                .all(|hit| hit.chunk_id != "a-old-1" && hit.chunk_id != "a-old-2")
        );
    }

    #[test]
    fn rebuild_from_replaces_everything() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "junk.md",
            "Junk",
            &[chunk("junk-1", "junk.md", 0, None, "junk 傅里叶 content")],
        )
        .expect("junk upsert");
        writer.commit().expect("junk commit");

        let fresh = vec![
            chunk("r-1", "r/a.md", 0, None, "alpha beta"),
            chunk("r-2", "r/b.md", 1, Some("深度"), "深度学习模型"),
        ];
        let mut titles = HashMap::new();
        titles.insert("r/a.md".to_string(), "Alpha".to_string());
        titles.insert("r/b.md".to_string(), "深度".to_string());
        let count =
            SearchIndex::rebuild_from(&mut writer, si.fields(), &fresh, &titles).expect("rebuild");
        assert_eq!(count, 2);
        writer.commit().expect("rebuild commit");

        assert!(si.search("junk", 10).expect("junk gone").is_empty());
        assert!(si.search("傅里叶", 10).expect("old gone").is_empty());
        let en = si.search("alpha", 10).expect("alpha");
        assert_eq!(en.len(), 1);
        assert_eq!(en[0].chunk_id, "r-1");
        assert_eq!(en[0].heading_path, None);
        let zh = si.search("深度", 10).expect("zh");
        assert_eq!(zh.len(), 1);
        assert_eq!(zh[0].chunk_id, "r-2");
    }

    #[test]
    fn top_k_limits_results_and_ranks_from_one() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        let chunks: Vec<Chunk> = (0..5)
            .map(|i| {
                chunk(
                    &format!("k-{i}"),
                    &format!("k/{i}.md"),
                    i,
                    None,
                    &format!("common token number {i} extra{i}"),
                )
            })
            .collect();
        SearchIndex::upsert_source(&mut writer, si.fields(), "k/all.md", "K", &chunks)
            .expect("upsert");
        writer.commit().expect("commit");

        let hits = si.search("common token", 3).expect("hits");
        assert_eq!(hits.len(), 3);
        for (i, hit) in hits.iter().enumerate() {
            assert_eq!(hit.rank, i as u32 + 1);
        }
    }

    #[test]
    fn escaped_plus_query_finds_cpp_doc() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "cpp.md",
            "C++ Notes",
            &[chunk(
                "cpp-1",
                "cpp.md",
                0,
                None,
                "C++ template metaprogramming examples",
            )],
        )
        .expect("upsert");
        writer.commit().expect("commit");

        let hits = si.search("C++ template", 10).expect("search");
        assert!(!hits.is_empty(), "escaped C++ query must hit the cpp doc");
        assert_eq!(hits[0].chunk_id, "cpp-1");
    }

    #[test]
    fn disk_index_reopens_with_existing_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idx");
        {
            let si = SearchIndex::open_or_create(&path).expect("create");
            let mut writer = si.writer(20_000_000).expect("writer");
            SearchIndex::upsert_source(
                &mut writer,
                si.fields(),
                "d/a.md",
                "A",
                &[chunk("d-1", "d/a.md", 0, None, "persistent 傅里叶 doc")],
            )
            .expect("upsert");
            writer.commit().expect("commit");
        }
        assert!(path.join("meta.json").exists());
        let si = SearchIndex::open_or_create(&path).expect("reopen");
        let hits = si.search("傅里叶", 10).expect("search after reopen");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, "d-1");
    }

    #[test]
    fn tie_break_sorts_equal_scores_by_source_then_ordinal() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        // Identical single-term docs across sources get identical BM25 scores
        // (same term, same length, same field stats).
        let sources = ["z.md", "a.md", "m.md"];
        for (i, key) in sources.iter().enumerate() {
            SearchIndex::upsert_source(
                &mut writer,
                si.fields(),
                key,
                "T",
                &[chunk(
                    &format!("t-{i}"),
                    key,
                    0,
                    None,
                    "identical body text",
                )],
            )
            .expect("upsert");
        }
        writer.commit().expect("commit");
        let hits = si.search("identical body", 10).expect("hits");
        assert_eq!(hits.len(), 3);
        let keys: Vec<&str> = hits.iter().map(|h| h.source_key.as_str()).collect();
        assert_eq!(keys, ["a.md", "m.md", "z.md"]);
        for (i, hit) in hits.iter().enumerate() {
            assert_eq!(hit.rank, i as u32 + 1);
        }
    }

    /// Bare boolean keywords are alphanumeric, so they pass the early-out;
    /// tantivy's grammar may reject the escaped form. The search must
    /// either match or return empty — never surface a parse error.
    #[test]
    fn bare_keyword_queries_never_error() {
        let si = ram_index();
        let mut writer = si.writer(20_000_000).expect("writer");
        SearchIndex::upsert_source(
            &mut writer,
            si.fields(),
            "a.md",
            "A",
            &[chunk("a-1", "a.md", 0, None, "plain content here")],
        )
        .expect("upsert");
        writer.commit().expect("commit");

        for query in ["OR", "AND NOT", "or", "the OR and", "NOT"] {
            let result = si.search(query, 10);
            assert!(result.is_ok(), "query {query:?} must not error");
        }
    }

    /// A foreign/stale index directory must surface a structured error,
    /// never a panic, when its schema lacks our fields.
    #[test]
    fn foreign_index_schema_reports_mismatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_path_buf();
        let foreign = tantivy::Index::create_in_dir(&dir, {
            let mut builder = tantivy::schema::SchemaBuilder::default();
            builder.add_text_field("unrelated", tantivy::schema::STRING);
            builder.build()
        })
        .expect("create foreign index");
        drop(foreign);

        let error = match SearchIndex::open_or_create(&dir) {
            Err(error) => error,
            Ok(_) => panic!("foreign index must fail with SchemaMismatch"),
        };
        assert!(
            matches!(error, RetrieveError::SchemaMismatch(_)),
            "expected SchemaMismatch, got {error:?}"
        );
    }
}
