//! Index lifecycle, document writes, and BM25 search execution.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use tantivy::collector::TopDocs;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{Field, Value};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use momotaro_contracts::{Chunk, OriginClass, RetrievalHit};

use crate::RetrieveError;
use crate::boost;
use crate::cjk::{CJK_TOKENIZER_NAME, cjk_analyzer};
use crate::query::{escape_query, is_searchable, normalize_query};
use crate::schema::{self, Fields};

/// Sentinel file tantivy writes on index creation.
const META_FILE: &str = "meta.json";

/// An opened Tantivy index with the Momotaro schema and `cjk` tokenizer.
pub struct SearchIndex {
    index: Index,
    fields: Fields,
    reader: IndexReader,
    parser: OnceLock<QueryParser>,
    parse_fell_back: AtomicBool,
}

/// Thin newtype over [`tantivy::IndexWriter`].
///
/// Writes are uncommitted; the caller drives [`IndexWriterHandle::commit`].
pub struct IndexWriterHandle {
    pub(crate) inner: IndexWriter<TantivyDocument>,
}

impl IndexWriterHandle {
    /// Commits all queued operations, making them visible to searchers.
    pub fn commit(&mut self) -> Result<(), RetrieveError> {
        self.inner.commit()?;
        Ok(())
    }

    /// Waits for every worker and merge thread, then drops the writer.
    ///
    /// Dropping a writer is not joining it: tantivy keeps workers and merges
    /// running after the value goes out of scope, and on Windows a directory one
    /// of them still has open cannot be renamed. A rebuild that swaps its staging
    /// directory in failed with a bare `os error 5` at ten thousand notes for
    /// exactly that reason; the handle was dropped at the end of a block and the
    /// merge was still ~1.4 s from finishing. Consumes the handle, as
    /// `wait_merging_threads` itself does.
    pub fn finish(self) -> Result<(), RetrieveError> {
        self.inner.wait_merging_threads()?;
        Ok(())
    }
}

/// One scored document pulled from the store, before rank assignment.
struct Row {
    score: f64,
    source_key: String,
    revision_hash: String,
    chunk_id: String,
    chunk_hash: String,
    ordinal: u32,
    heading_path: Option<String>,
    locator_json: String,
}

impl Row {
    fn into_hit(self, rank: u32) -> RetrievalHit {
        RetrievalHit {
            rank,
            source_key: self.source_key,
            revision_hash: self.revision_hash,
            chunk_id: self.chunk_id,
            chunk_hash: self.chunk_hash,
            ordinal: self.ordinal,
            heading_path: self.heading_path,
            score: self.score,
            locator_json: self.locator_json,
        }
    }
}

impl SearchIndex {
    /// Whether a tantivy index directory already exists at `dir`.
    ///
    /// Owns the sentinel-file knowledge so no other crate has to know what
    /// tantivy writes on disk.
    pub fn exists(dir: &Path) -> bool {
        dir.join(META_FILE).exists()
    }

    /// Builds an index directly from parts. Test-only.
    #[cfg(test)]
    pub(crate) fn from_parts(index: Index, fields: Fields, reader: IndexReader) -> Self {
        Self {
            index,
            fields,
            reader,
            parser: OnceLock::new(),
            parse_fell_back: AtomicBool::new(false),
        }
    }

    /// Opens an existing index in `dir` or creates a new one.
    ///
    /// The `cjk` tokenizer is registered on both paths. The directory is
    /// created when absent. A stale or foreign on-disk index surfaces as
    /// [`RetrieveError::SchemaMismatch`] instead of panicking.
    pub fn open_or_create(dir: &Path) -> Result<SearchIndex, RetrieveError> {
        let has_meta = Self::exists(dir);
        let index = if has_meta {
            Index::open_in_dir(dir)?
        } else {
            std::fs::create_dir_all(dir)?;
            Index::create_in_dir(dir, schema::build_schema())?
        };
        index
            .tokenizers()
            .register(CJK_TOKENIZER_NAME, cjk_analyzer());
        let fields = schema::fields(&index.schema())?;
        let reader = index.reader()?;
        Ok(SearchIndex {
            index,
            fields,
            reader,
            parser: OnceLock::new(),
            parse_fell_back: AtomicBool::new(false),
        })
    }

    /// Schema field handles for this index.
    pub fn fields(&self) -> &Fields {
        &self.fields
    }

    /// Whether the last [`Self::search`] exhausted both query parses and
    /// returned empty. Callers carrying a retrieval report can record it;
    /// `false` is meaningful only for the immediately preceding call.
    pub fn parse_fell_back(&self) -> bool {
        self.parse_fell_back.load(AtomicOrdering::Relaxed)
    }

    /// The underlying index; test-only (the tests that read stored fields and
    /// class terms).
    #[cfg(test)]
    pub(crate) fn index(&self) -> &Index {
        &self.index
    }

    /// Creates a writer with the given total memory budget in bytes.
    pub fn writer(&self, heap_bytes: usize) -> Result<IndexWriterHandle, RetrieveError> {
        Ok(IndexWriterHandle {
            inner: self.index.writer(heap_bytes)?,
        })
    }

    /// How many documents the index currently serves.
    ///
    /// The count the run layer compares against the store's current-chunk
    /// count: the writer commits a batch's documents in one step after the
    /// per-file store commits, so the two numbers disagree exactly when a
    /// batch died in that window. The reader sees the last committed
    /// generation.
    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Replaces all indexed chunks for one source with `chunks`.
    ///
    /// Deletes every document tagged with `source_key`, then adds one
    /// document per chunk. Nothing is committed; call
    /// [`IndexWriterHandle::commit`] once after a batch.
    pub fn upsert_source(
        writer: &mut IndexWriterHandle,
        fields: &Fields,
        source_key: &str,
        title: &str,
        origin: OriginClass,
        chunks: &[Chunk],
    ) -> Result<(), RetrieveError> {
        writer
            .inner
            .delete_term(Term::from_field_text(fields.source_key, source_key));
        for chunk in chunks {
            let doc = schema::doc_from_chunk(fields, title, origin, chunk);
            writer.inner.add_document(doc)?;
        }
        Ok(())
    }

    /// Drops every indexed chunk of one source.
    ///
    /// The derived half of a deletion: SQLite keeps the revision as history
    /// (ADR 0014) and `graph_links` are pruned in the same batch, but a
    /// document left in Tantivy would keep answering searches for a file that
    /// is gone — which is the one place "eventually consistent" would be a
    /// lie the user can see. Nothing is committed; call
    /// [`IndexWriterHandle::commit`] once after a batch.
    pub fn remove_source(
        writer: &mut IndexWriterHandle,
        fields: &Fields,
        source_key: &str,
    ) -> Result<(), RetrieveError> {
        writer
            .inner
            .delete_term(Term::from_field_text(fields.source_key, source_key));
        Ok(())
    }

    /// Rebuilds the whole index from a full chunk set.
    ///
    /// Deletes all documents, then indexes every chunk. Titles and trust classes
    /// are looked up per `chunk.source_key` (an empty title is fine; a missing
    /// class is refused — guessing one would silently mis-weight the source).
    /// Returns the number of chunks queued. Nothing is committed.
    pub fn rebuild_from(
        writer: &mut IndexWriterHandle,
        fields: &Fields,
        chunks: &[Chunk],
        titles: &HashMap<String, String>,
        origins: &HashMap<String, OriginClass>,
    ) -> Result<u64, RetrieveError> {
        writer.inner.delete_all_documents()?;
        for chunk in chunks {
            let title = titles
                .get(&chunk.source_key)
                .map(String::as_str)
                .unwrap_or("");
            let origin = origins
                .get(&chunk.source_key)
                .copied()
                .ok_or_else(|| RetrieveError::MissingOrigin(chunk.source_key.clone()))?;
            let doc = schema::doc_from_chunk(fields, title, origin, chunk);
            writer.inner.add_document(doc)?;
        }
        Ok(chunks.len() as u64)
    }

    /// Runs a BM25 query across `text`, `title`, and `heading_path`.
    ///
    /// The query is whitespace-normalized and syntax-escaped before
    /// parsing, so user input is always treated as literal text. Queries
    /// without any alphanumeric content (empty or punctuation-only) return
    /// an empty hit list rather than an error. A query whose escaped form
    /// still fails to parse (e.g. a bare `OR` / `AND` / `NOT` keyword) is
    /// retried once with every whitespace-separated term quoted; if that
    /// also fails, the search returns an empty hit list and raises the
    /// observable [`Self::parse_fell_back`] marker — a query that cannot be
    /// parsed is not a match, but its fallback must be visible. The internal reader is
    /// reloaded first, so the last commit of this process is visible
    /// without any external reader handle. Equal scores tie-break by
    /// `source_key` ascending, then `ordinal` ascending; `rank` is
    /// 1-based.
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<RetrievalHit>, RetrieveError> {
        self.parse_fell_back.store(false, AtomicOrdering::Relaxed);
        let normalized = normalize_query(query);
        // Two halves, for two different reasons. `!is_searchable` is the
        // predicate `--mode expand` shares (see `is_searchable`), and the
        // parser's own empty-result path would cover it if it were removed.
        // `top_k == 0` is load-bearing instead: `TopDocs::with_limit(0)`
        // panics (`assert_ne!(limit, 0, "Limit must be greater than 0")`)
        // and the CLI takes `--top 0`. Neither half is removable.
        if top_k == 0 || !is_searchable(&normalized) {
            return Ok(Vec::new());
        }
        self.reader.reload()?;
        let parser = self.parser.get_or_init(|| {
            QueryParser::for_index(
                &self.index,
                vec![
                    self.fields.text,
                    self.fields.title,
                    self.fields.heading_path,
                ],
            )
        });
        let escaped = escape_query(&normalized);
        let (parsed, fell_back) = parse_query_with_fallback(parser, &escaped, &normalized);
        self.parse_fell_back
            .store(fell_back, AtomicOrdering::Relaxed);
        let Some(parsed) = parsed else {
            return Ok(Vec::new());
        };
        // Class scoping is the ranking backbone, not a filter bolted on after:
        // every hit is produced by exactly one weighted class branch.
        let parsed = boost::origin_scoped(&self.fields, parsed);
        let searcher = self.reader.searcher();
        // TopDocs' own limit cuts ties before the documented tie-break can
        // see them. Collect every match, apply the full total order, then cut.
        let all_matches = searcher.num_docs().max(1) as usize;
        let top_docs =
            searcher.search(&parsed, &TopDocs::with_limit(all_matches).order_by_score())?;

        let mut rows: Vec<Row> = Vec::with_capacity(top_docs.len());
        for (score, address) in top_docs {
            let doc: TantivyDocument = searcher.doc(address)?;
            rows.push(Row {
                score: f64::from(score),
                source_key: stored_text(&doc, self.fields.source_key),
                revision_hash: stored_text(&doc, self.fields.revision_hash),
                chunk_id: stored_text(&doc, self.fields.chunk_id),
                chunk_hash: stored_text(&doc, self.fields.chunk_hash),
                ordinal: stored_ordinal(&doc, self.fields.ordinal)?,
                heading_path: stored_optional_text(&doc, self.fields.heading_path),
                locator_json: stored_text(&doc, self.fields.locator_json),
            });
        }
        rows.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.source_key.cmp(&b.source_key))
                .then_with(|| a.ordinal.cmp(&b.ordinal))
        });
        rows.truncate(top_k);
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(position, row)| {
                let rank = u32::try_from(position)
                    .unwrap_or(u32::MAX)
                    .saturating_add(1);
                row.into_hit(rank)
            })
            .collect())
    }
}

/// Parse the literal syntax, then the all-terms-quoted retry. The second bool
/// is the observable fallback: `None` is an empty search, not a lexical miss.
fn parse_query_with_fallback(
    parser: &QueryParser,
    escaped: &str,
    raw: &str,
) -> (Option<Box<dyn Query>>, bool) {
    if let Ok(parsed) = parser.parse_query(escaped) {
        return (Some(parsed), false);
    }
    let quoted = raw
        .split_whitespace()
        .map(|term| format!("\"{}\"", escape_query(term)))
        .collect::<Vec<_>>()
        .join(" ");
    match parser.parse_query(&quoted) {
        Ok(parsed) => (Some(parsed), false),
        Err(_) => (None, true),
    }
}

fn stored_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn stored_optional_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    let text = doc.get_first(field).and_then(|value| value.as_str())?;
    (!text.is_empty()).then(|| text.to_string())
}

fn stored_ordinal(doc: &TantivyDocument, field: Field) -> Result<u32, RetrieveError> {
    doc.get_first(field)
        .and_then(|value| value.as_i64())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            RetrieveError::Tantivy(tantivy::TantivyError::SchemaError(
                "stored ordinal is missing or outside u32".to_string(),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_parser_failures_are_a_fallback_and_one_success_is_not() {
        let si = crate::tests::ram_index();
        // With no default fields, both the literal and phrase retry fail at
        // parse time; this is the smallest way to drive the retry arm.
        let none = QueryParser::for_index(si.index(), Vec::new());
        let (query, fell_back) = parse_query_with_fallback(&none, "plain", "plain");
        assert!(query.is_none());
        assert!(fell_back, "both parser attempts failed");

        let parser = QueryParser::for_index(
            si.index(),
            vec![
                si.fields().text,
                si.fields().title,
                si.fields().heading_path,
            ],
        );
        let (query, fell_back) = parse_query_with_fallback(&parser, "plain", "plain");
        assert!(query.is_some());
        assert!(!fell_back);
    }

    /// A schema can promise an i64 ordinal while a damaged stored document
    /// lacks it. The decoder must fail the search, never synthesize position 0.
    #[test]
    fn a_missing_stored_ordinal_is_an_error_not_zero() {
        let schema = crate::schema::build_schema();
        let fields = crate::schema::fields(&schema).expect("own schema");
        let doc = TantivyDocument::default();

        let error = stored_ordinal(&doc, fields.ordinal).expect_err("missing ordinal");
        assert!(
            matches!(
                error,
                RetrieveError::Tantivy(tantivy::TantivyError::SchemaError(_))
            ),
            "{error:?}"
        );
    }

    #[test]
    fn repeated_searches_reset_the_parse_fallback_marker() {
        let si = crate::tests::ram_index();
        si.parse_fell_back.store(true, AtomicOrdering::Relaxed);
        assert!(si.parse_fell_back());
        assert!(si.search("nothing here", 10).expect("search").is_empty());
        assert!(
            !si.parse_fell_back(),
            "a successful parse clears the marker"
        );
    }
}
