//! Index schema mapping `Chunk` records onto tantivy fields.

use tantivy::TantivyDocument;
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, SchemaBuilder, TextFieldIndexing,
    TextOptions,
};

use momotaro_contracts::{Chunk, OriginClass};

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
    /// Indexed body: `chunks.text` with the `《title》 > heading path：` prefix
    /// (see [`indexed_text`]); TEXT with the `cjk` tokenizer, indexed only — the
    /// canonical text lives in SQLite and is never read back from here.
    pub text: Field,
    /// JSON locator; raw-tokenized and indexed like any `STRING` field, plus
    /// stored. Kept opaque: it is carried for the caller, not interpreted here.
    pub locator_json: Field,
    /// Trust origin of the owning source (`owner`, `paper`, … from
    /// [`OriginClass::as_str`]); raw-tokenized — a class is a keyword, not prose —
    /// indexed **and stored**, so a reader can get the class back.
    pub origin_class: Field,
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
    let _origin_class = builder.add_text_field("origin_class", STRING | STORED);
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
        origin_class: get("origin_class")?,
    })
}

/// Converts one chunk into a tantivy document.
///
/// `title` is the display title of the owning source and `origin` its trust
/// class. A missing `heading_path` is stored as an empty string and restored as
/// `None` on retrieval. The indexed body carries the heading-chain prefix (see
/// [`indexed_text`]); the canonical `chunk.text` is never modified here.
pub fn doc_from_chunk(
    fields: &Fields,
    title: &str,
    origin: OriginClass,
    chunk: &Chunk,
) -> TantivyDocument {
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
    doc.add_text(fields.text, indexed_text(title, chunk));
    doc.add_text(fields.locator_json, &chunk.locator_json);
    doc.add_text(fields.origin_class, origin.as_str());
    doc
}

/// The searchable form of one chunk: `《title》 > heading_path：body`.
///
/// Index-only. The canonical `chunks.text` stays the verbatim source slice, so
/// citation verification always resolves against the original bytes rather than
/// against this prefix. Missing pieces degrade in one direction only — no
/// heading gives `《title》：body`; no title gives `heading_path：body`; neither
/// gives the bare body, which is the pre-prefix form.
pub fn indexed_text(title: &str, chunk: &Chunk) -> String {
    let heading = chunk.heading_path.as_deref().unwrap_or("");
    match (title.is_empty(), heading.is_empty()) {
        (false, false) => format!("《{title}》 > {heading}：{}", chunk.text),
        (false, true) => format!("《{title}》：{}", chunk.text),
        (true, false) => format!("{heading}：{}", chunk.text),
        (true, true) => chunk.text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use momotaro_contracts::{Chunk, OriginClass};
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
        let doc = doc_from_chunk(&f, "Fourier Notes", OriginClass::Owner, &chunk);
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
        assert_eq!(
            stored(f.text),
            "《Fourier Notes》 > Fourier > 性质：傅里叶变换 is a transform",
            "the indexed body carries the heading chain prefix"
        );
        assert_eq!(stored(f.origin_class), "owner");
        assert_eq!(stored(f.locator_json), r#"{"start_byte":0}"#);
        assert_eq!(doc.get_first(f.ordinal).expect("ordinal").as_i64(), Some(3));
        assert_eq!(
            chunk.text, "傅里叶变换 is a transform",
            "the canonical text is never rewritten"
        );
    }

    #[test]
    fn index_text_carries_heading_chain_prefix() {
        let schema = build_schema();
        let f = fields(&schema).expect("own schema resolves");
        let chunk = Chunk {
            heading_path: Some("傅里叶 > 性质".to_string()),
            text: "线性性质。".to_string(),
            ..sample_chunk()
        };

        let indexed = indexed_text("傅里叶变换", &chunk);
        assert_eq!(indexed, "《傅里叶变换》 > 傅里叶 > 性质：线性性质。");

        let doc = doc_from_chunk(&f, "傅里叶变换", OriginClass::Owner, &chunk);
        assert_eq!(
            doc.get_first(f.text).and_then(|value| value.as_str()),
            Some(indexed.as_str()),
            "the document indexes exactly that text"
        );
        assert_eq!(chunk.text, "线性性质。", "canonical text unchanged");
    }

    #[test]
    fn missing_title_or_heading_degrade_in_one_direction() {
        let chunk = sample_chunk();
        let bare = Chunk {
            heading_path: None,
            ..sample_chunk()
        };

        assert_eq!(
            indexed_text("傅里叶变换", &bare),
            "《傅里叶变换》：傅里叶变换 is a transform"
        );
        assert_eq!(
            indexed_text("", &chunk),
            "Fourier > 性质：傅里叶变换 is a transform"
        );
        assert_eq!(
            indexed_text("", &bare),
            "傅里叶变换 is a transform",
            "no title and no heading is the pre-prefix form"
        );
    }

    #[test]
    fn the_class_is_indexed_and_stored() {
        // The name must be true of the *schema*, not of an in-memory document:
        // `doc.add_text` returns whatever was added regardless of the field's
        // options, so a round trip through that document proves nothing about
        // `stored` (tantivy 0.26.2 `STRING` is indexed but `stored: false`).
        let schema = build_schema();
        let f = fields(&schema).expect("own schema resolves");
        let entry = schema.get_field_entry(f.origin_class);
        let options = match entry.field_type() {
            tantivy::schema::FieldType::Str(options) => options,
            other => panic!("origin_class must be a text field, got {other:?}"),
        };
        assert!(options.is_stored(), "readable back, not just searchable");
        let indexing = options
            .get_indexing_options()
            .expect("searchable by its wire spelling");
        assert_eq!(
            indexing.tokenizer(),
            "raw",
            "a class is a keyword, not prose"
        );

        for (class, spelling) in [
            (OriginClass::Owner, "owner"),
            (OriginClass::Paper, "paper"),
            (OriginClass::Web, "web"),
            (OriginClass::Agent, "agent"),
            (OriginClass::System, "system"),
        ] {
            let doc = doc_from_chunk(&f, "T", class, &sample_chunk());
            assert_eq!(
                doc.get_first(f.origin_class).and_then(|v| v.as_str()),
                Some(spelling),
                "{class:?} is written by its wire spelling"
            );
        }
    }

    #[test]
    fn missing_heading_path_stores_empty_string() {
        let schema = build_schema();
        let f = fields(&schema).expect("own schema resolves");
        let mut chunk = sample_chunk();
        chunk.heading_path = None;
        let doc = doc_from_chunk(&f, "T", OriginClass::Owner, &chunk);
        let value = doc
            .get_first(f.heading_path)
            .expect("heading_path present")
            .as_str()
            .expect("string")
            .to_string();
        assert_eq!(value, "");
    }

    #[test]
    fn a_schema_without_origin_class_is_a_mismatch() {
        // An index written by an older build — everything except the new field —
        // must surface as "rebuild", never as a panic or as silently unweighted
        // results.
        let mut older = SchemaBuilder::default();
        older.add_text_field("source_key", STRING | STORED);
        older.add_text_field("revision_hash", STRING | STORED);
        older.add_text_field("chunk_id", STRING | STORED);
        older.add_text_field("chunk_hash", STORED);
        older.add_i64_field("ordinal", INDEXED | STORED);
        older.add_text_field("title", cjk_text_options() | STORED);
        older.add_text_field("heading_path", cjk_text_options() | STORED);
        older.add_text_field("text", cjk_text_options());
        older.add_text_field("locator_json", STRING | STORED);

        let error = fields(&older.build()).expect_err("missing field is a mismatch");
        assert!(
            matches!(error, crate::RetrieveError::SchemaMismatch(ref name) if name == "origin_class"),
            "{error}"
        );
    }
}
