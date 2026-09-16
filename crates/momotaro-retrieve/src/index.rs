//! Index lifecycle, document writes, and BM25 search execution.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Value};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use momotaro_contracts::{Chunk, RetrievalHit};

use crate::RetrieveError;
use crate::cjk::{CJK_TOKENIZER_NAME, cjk_analyzer};
use crate::query::{escape_query, normalize_query};
use crate::schema::{self, Fields};

/// Sentinel file tantivy writes on index creation.
const META_FILE: &str = "meta.json";

/// An opened Tantivy index with the Momotaro schema and `cjk` tokenizer.
pub struct SearchIndex {
    index: Index,
    fields: Fields,
    reader: IndexReader,
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
        })
    }

    /// Schema field handles for this index.
    pub fn fields(&self) -> &Fields {
        &self.fields
    }

    /// Creates a writer with the given total memory budget in bytes.
    pub fn writer(&self, heap_bytes: usize) -> Result<IndexWriterHandle, RetrieveError> {
        Ok(IndexWriterHandle {
            inner: self.index.writer(heap_bytes)?,
        })
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
        chunks: &[Chunk],
    ) -> Result<(), RetrieveError> {
        writer
            .inner
            .delete_term(Term::from_field_text(fields.source_key, source_key));
        for chunk in chunks {
            let doc = schema::doc_from_chunk(fields, title, chunk);
            writer.inner.add_document(doc)?;
        }
        Ok(())
    }

    /// Rebuilds the whole index from a full chunk set.
    ///
    /// Deletes all documents, then indexes every chunk. Titles are looked
    /// up per `chunk.source_key` (empty string when absent). Returns the
    /// number of chunks queued. Nothing is committed.
    pub fn rebuild_from(
        writer: &mut IndexWriterHandle,
        fields: &Fields,
        chunks: &[Chunk],
        titles: &HashMap<String, String>,
    ) -> Result<u64, RetrieveError> {
        writer.inner.delete_all_documents()?;
        for chunk in chunks {
            let title = titles
                .get(&chunk.source_key)
                .map(String::as_str)
                .unwrap_or("");
            let doc = schema::doc_from_chunk(fields, title, chunk);
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
    /// also fails, the search returns an empty hit list — a query that
    /// cannot match anything is not an error. The internal reader is
    /// reloaded first, so the last commit of this process is visible
    /// without any external reader handle. Equal scores tie-break by
    /// `source_key` ascending, then `ordinal` ascending; `rank` is
    /// 1-based.
    pub fn search(&self, query: &str, top_k: usize) -> Result<Vec<RetrievalHit>, RetrieveError> {
        let normalized = normalize_query(query);
        if top_k == 0 || !normalized.chars().any(char::is_alphanumeric) {
            return Ok(Vec::new());
        }
        self.reader.reload()?;
        let parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.text,
                self.fields.title,
                self.fields.heading_path,
            ],
        );
        let escaped = escape_query(&normalized);
        let parsed = match parser.parse_query(&escaped) {
            Ok(parsed) => parsed,
            Err(_) => {
                let quoted = normalized
                    .split_whitespace()
                    .map(|term| format!("\"{}\"", escape_query(term)))
                    .collect::<Vec<_>>()
                    .join(" ");
                match parser.parse_query(&quoted) {
                    Ok(parsed) => parsed,
                    Err(_) => return Ok(Vec::new()),
                }
            }
        };
        let searcher = self.reader.searcher();
        let top_docs = searcher.search(&parsed, &TopDocs::with_limit(top_k).order_by_score())?;

        let mut rows: Vec<Row> = Vec::with_capacity(top_docs.len());
        for (score, address) in top_docs {
            let doc: TantivyDocument = searcher.doc(address)?;
            rows.push(Row {
                score: f64::from(score),
                source_key: stored_text(&doc, self.fields.source_key),
                revision_hash: stored_text(&doc, self.fields.revision_hash),
                chunk_id: stored_text(&doc, self.fields.chunk_id),
                chunk_hash: stored_text(&doc, self.fields.chunk_hash),
                ordinal: stored_ordinal(&doc, self.fields.ordinal),
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

fn stored_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn stored_optional_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    let text = doc.get_first(field).and_then(|value| value.as_str())?;
    (!text.is_empty()).then(|| text.to_string())
}

fn stored_ordinal(doc: &TantivyDocument, field: Field) -> u32 {
    doc.get_first(field)
        .and_then(|value| value.as_i64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}
