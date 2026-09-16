//! Index schema mapping `Chunk` records onto tantivy fields.

use tantivy::TantivyDocument;
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, SchemaBuilder, TextFieldIndexing,
    TextOptions,
};

use momotaro_contracts::Chunk;

/// The fields of the retrieval schema, resolved from a [`Schema`].
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    /// Owning source key; STRING, indexed + stored.
    pub source_key: Field,
    /// Owning revision hash; STRING, indexed + stored.
    pub revision_hash: Field,
    /// Global chunk identifier; STRING, indexed + stored.
    pub chunk_id: Field,
    /// Chunk content hash; STRING, stored only (not indexed).
    pub chunk_hash: Field,
    /// Position within the revision; i64, indexed + stored.
    pub ordinal: Field,
    /// Source display title; TEXT with the `cjk` tokenizer, indexed + stored.
    pub title: Field,
    /// Heading path; TEXT with the `cjk` tokenizer, indexed + stored.
    pub heading_path: Field,
    /// Chunk body; TEXT with the `cjk` tokenizer, indexed only (canonical
    /// text lives in SQLite).
    pub text: Field,
    /// JSON locator; STRING, stored only (not indexed).
    pub locator_json: Field,
}

fn cjk_text_options() -> TextOptions {
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(crate::cjk::CJK_TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    TextOptions::default().set_indexing_options(indexing)
}

/// Builds the retrieval schema. Call once, then resolve [`fields`].
pub fn build_schema() -> Schema {
    let mut builder = SchemaBuilder::default();
    let _source_key = builder.add_text_field("source_key", STRING | STORED);
    let _revision_hash = builder.add_text_field("revision_hash", STRING | STORED);
    let _chunk_id = builder.add_text_field("chunk_id", STRING | STORED);
    let _chunk_hash = builder.add_text_field("chunk_hash", STORED);
    let _ordinal = builder.add_i64_field("ordinal", INDEXED | STORED);
    let _title = builder.add_text_field("title", cjk_text_options() | STORED);
    let _heading_path = builder.add_text_field("heading_path", cjk_text_options() | STORED);
    let _text = builder.add_text_field("text", cjk_text_options());
    let _locator_json = builder.add_text_field("locator_json", STRING | STORED);
    builder.build()
}

/// Resolves the named fields from a schema built by [`build_schema`].
///
/// Returns [`RetrieveError::SchemaMismatch`] for schemas missing any
/// expected field — e.g. an on-disk index written by another build. Callers
/// should surface this as "rebuild the index", never panic.
pub fn fields(schema: &Schema) -> Result<Fields, crate::RetrieveError> {
    let get = |name: &str| {
        schema
            .get_field(name)
            .map_err(|_| crate::RetrieveError::SchemaMismatch(name.to_string()))
    };
    Ok(Fields {
        source_key: get("source_key")?,
        revision_hash: get("revision_hash")?,
        chunk_id: get("chunk_id")?,
        chunk_hash: get("chunk_hash")?,
        ordinal: get("ordinal")?,
        title: get("title")?,
        heading_path: get("heading_path")?,
        text: get("text")?,
        locator_json: get("locator_json")?,
    })
}

/// Converts one chunk into a tantivy document.
///
/// `title` is the display title of the owning source. A missing
/// `heading_path` is stored as an empty string and restored as `None` on
/// retrieval.
pub fn doc_from_chunk(fields: &Fields, title: &str, chunk: &Chunk) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_text(fields.source_key, &chunk.source_key);
    doc.add_text(fields.revision_hash, &chunk.revision_hash);
    doc.add_text(fields.chunk_id, &chunk.chunk_id);
    doc.add_text(fields.chunk_hash, &chunk.chunk_hash);
    doc.add_i64(fields.ordinal, i64::from(chunk.ordinal));
    doc.add_text(fields.title, title);
    doc.add_text(
        fields.heading_path,
        chunk.heading_path.as_deref().unwrap_or(""),
    );
    doc.add_text(fields.text, &chunk.text);
    doc.add_text(fields.locator_json, &chunk.locator_json);
    doc
}

#[cfg(test)]
mod tests {
    use momotaro_contracts::Chunk;
    use tantivy::schema::Value;

    use super::*;

    fn sample_chunk() -> Chunk {
        Chunk {
            chunk_id: "chunk-1".to_string(),
            source_key: "ml/fourier.md".to_string(),
            revision_hash: "rev-abc".to_string(),
            ordinal: 3,
            heading_path: Some("Fourier > 性质".to_string()),
            text: "傅里叶变换 is a transform".to_string(),
            locator_json: r#"{"start_byte":0}"#.to_string(),
            chunk_hash: "hash-1".to_string(),
            token_estimate: 12,
        }
    }

    #[test]
    fn schema_fields_resolve_by_name() {
        let schema = build_schema();
        let f = fields(&schema).expect("own schema resolves");
        assert_eq!(schema.get_field_name(f.source_key), "source_key");
        assert_eq!(schema.get_field_name(f.text), "text");
        assert_eq!(schema.get_field_name(f.locator_json), "locator_json");
    }

    #[test]
    fn doc_round_trips_stored_fields() {
        let schema = build_schema();
        let f = fields(&schema).expect("own schema resolves");
        let chunk = sample_chunk();
        let doc = doc_from_chunk(&f, "Fourier Notes", &chunk);
        let stored = |field: Field| {
            doc.get_first(field)
                .expect("field stored")
                .as_str()
                .expect("string value")
                .to_string()
        };
        assert_eq!(stored(f.source_key), "ml/fourier.md");
        assert_eq!(stored(f.revision_hash), "rev-abc");
        assert_eq!(stored(f.chunk_id), "chunk-1");
        assert_eq!(stored(f.chunk_hash), "hash-1");
        assert_eq!(stored(f.title), "Fourier Notes");
        assert_eq!(stored(f.heading_path), "Fourier > 性质");
        assert_eq!(stored(f.text), "傅里叶变换 is a transform");
        assert_eq!(stored(f.locator_json), r#"{"start_byte":0}"#);
        assert_eq!(doc.get_first(f.ordinal).expect("ordinal").as_i64(), Some(3));
    }

    #[test]
    fn missing_heading_path_stores_empty_string() {
        let schema = build_schema();
        let f = fields(&schema).expect("own schema resolves");
        let mut chunk = sample_chunk();
        chunk.heading_path = None;
        let doc = doc_from_chunk(&f, "T", &chunk);
        let value = doc
            .get_first(f.heading_path)
            .expect("heading_path present")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(value, "");
    }
}
