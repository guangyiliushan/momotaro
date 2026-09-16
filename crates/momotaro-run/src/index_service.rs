//! Indexing, rebuilding, and searching the local vault.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use momotaro_contracts::{
    Chunk, IndexFileError, IndexPolicy, IndexReport, OriginClass, RetrievalHit, SourceKind,
};
use momotaro_ingest::{
    chunk_markdown, derive_title, normalize_markdown, revision_hash, source_key_for,
    walk_markdown_files,
};
use momotaro_retrieve::{IndexWriterHandle, SearchIndex};
use momotaro_store::{NewSourceRevision, Store};
use serde::{Deserialize, Serialize};

use crate::{RunError, WorkspaceConfig, database_path, load_config, validate_policy, vault_scope};

/// Tool tag embedded in every revision fingerprint.
const FINGERPRINT_TOOL: &str = "momotaro-index-v1";
/// Memory budget handed to the tantivy writer.
const WRITER_HEAP_BYTES: usize = 50_000_000;
/// Longest excerpt, in characters, attached to a search hit.
const EXCERPT_CHARS: usize = 200;

/// The tantivy index directory inside the workspace.
fn index_dir(root: &Path) -> PathBuf {
    root.join(".momotaro").join("index")
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Renders a path for durable display, stripping the Windows verbatim
/// prefix (`\\?\C:\...`) that `fs::canonicalize` produces.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// File mtime as epoch milliseconds; 0 when the clock cannot report it.
fn mtime_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn fingerprint_json(metadata: &fs::Metadata) -> String {
    serde_json::json!({
        "mtime_ms": mtime_ms(metadata),
        "size_bytes": metadata.len(),
        "tool": FINGERPRINT_TOOL,
    })
    .to_string()
}

/// Matches a stored fingerprint against the file's current mtime and size.
fn fingerprint_matches(metadata_json: &str, metadata: &fs::Metadata) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata_json) else {
        return false;
    };
    value.get("mtime_ms").and_then(serde_json::Value::as_u64) == Some(mtime_ms(metadata))
        && value.get("size_bytes").and_then(serde_json::Value::as_u64) == Some(metadata.len())
}

fn file_error(source_key: &str, message: impl std::fmt::Display) -> IndexFileError {
    IndexFileError {
        source_key: source_key.to_owned(),
        message: message.to_string(),
    }
}

/// Indexes the configured vault into the local store and tantivy index.
///
/// Unchanged files (same mtime+size fingerprint) are skipped; same-content
/// rewrites refresh the stored fingerprint without creating a revision;
/// changed or new files append a revision and replace their chunks. Per-file
/// failures are recorded in the report; the batch never aborts on one bad
/// file. The tantivy writer commits once at the end.
pub fn index_workspace(
    root: impl AsRef<Path>,
    path_override: Option<&Path>,
) -> Result<IndexReport, RunError> {
    let root = root.as_ref();
    let config: WorkspaceConfig = load_config(root)?;
    let started = Instant::now();

    let vault_root = match path_override {
        Some(override_path) => override_path.to_path_buf(),
        None => vault_scope(root, &config).root,
    };
    if !vault_root.is_dir() {
        return Err(RunError::VaultMissing { path: vault_root });
    }

    let policy = &config.index;
    validate_policy(policy)?;

    let mut store = Store::open(database_path(root))?;
    store.init_schema()?;

    fs::create_dir_all(index_dir(root)).map_err(|source| RunError::Io { source })?;
    let search_index = SearchIndex::open_or_create(&index_dir(root))?;
    let mut writer = search_index.writer(WRITER_HEAP_BYTES)?;

    let files = walk_markdown_files(&vault_root)?;

    let mut report = IndexReport {
        files_scanned: files.len() as u64,
        files_skipped: 0,
        files_indexed: 0,
        revisions_created: 0,
        chunks_written: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };

    for file in files {
        let source_key = match source_key_for(&vault_root, &file) {
            Ok(key) => key,
            Err(error) => {
                report.errors.push(file_error("<unknown>", error));
                continue;
            }
        };

        if let Err(message) = process_file(
            &mut store,
            &search_index,
            &mut writer,
            &file,
            &source_key,
            policy,
            &mut report,
        ) {
            report.errors.push(file_error(&source_key, message));
        }
    }

    writer.commit()?;

    report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(report)
}

/// Ingests one file into the store and the search index.
///
/// `Err` carries the per-file failure message; the caller records it and
/// moves on. Only batch-level failures (config, store open, walk, commit)
/// bubble up to [`index_workspace`].
#[allow(clippy::too_many_arguments)]
fn process_file(
    store: &mut Store,
    search_index: &SearchIndex,
    writer: &mut IndexWriterHandle,
    file: &Path,
    source_key: &str,
    policy: &IndexPolicy,
    report: &mut IndexReport,
) -> Result<(), String> {
    let metadata = fs::metadata(file).map_err(|error| error.to_string())?;
    let fingerprint = fingerprint_json(&metadata);

    let current = store
        .current_revision(source_key)
        .map_err(|error| error.to_string())?;

    if let Some(revision) = &current
        && fingerprint_matches(&revision.metadata_json, &metadata)
    {
        report.files_skipped += 1;
        return Ok(());
    }

    let bytes = fs::read(file).map_err(|error| error.to_string())?;
    let normalized = normalize_markdown(&bytes).map_err(|error| error.to_string())?;
    let hash = revision_hash(&normalized);

    if let Some(revision) = &current
        && revision.revision_hash == hash
    {
        store
            .update_revision_metadata(source_key, &revision.revision_hash, &fingerprint)
            .map_err(|error| error.to_string())?;
        report.files_indexed += 1;
        return Ok(());
    }

    let chunks = chunk_markdown(source_key, &hash, &normalized, policy);
    let title = derive_title(&normalized, source_key);
    let local_path = display_path(&fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf()));

    store
        .append_revision(&NewSourceRevision {
            source_key,
            revision_hash: &hash,
            kind: SourceKind::Note,
            title: Some(&title),
            uri: None,
            local_path: Some(&local_path),
            origin_class: OriginClass::Owner,
            metadata_json: &fingerprint,
            ingested_at: now_epoch_secs(),
        })
        .map_err(|error| error.to_string())?;
    store
        .replace_chunks(source_key, &hash, &chunks)
        .map_err(|error| error.to_string())?;

    SearchIndex::upsert_source(writer, search_index.fields(), source_key, &title, &chunks)
        .map_err(|error| error.to_string())?;

    report.files_indexed += 1;
    report.revisions_created += 1;
    report.chunks_written += chunks.len() as u64;
    Ok(())
}

/// Rebuilds the tantivy index from canonical store chunks.
///
/// Store rows are never touched; this only reconstructs the search index.
pub fn rebuild_index(root: impl AsRef<Path>) -> Result<RebuildReport, RunError> {
    let root = root.as_ref();
    let started = Instant::now();

    let mut store = Store::open(database_path(root))?;
    store.init_schema()?;

    let chunks = store.all_current_chunks()?;
    let titles = collect_titles(&store, &chunks)?;

    fs::create_dir_all(index_dir(root)).map_err(|source| RunError::Io { source })?;
    let search_index = SearchIndex::open_or_create(&index_dir(root))?;
    let mut writer = search_index.writer(WRITER_HEAP_BYTES)?;
    let indexed = SearchIndex::rebuild_from(&mut writer, search_index.fields(), &chunks, &titles)?;
    writer.commit()?;

    Ok(RebuildReport {
        chunks: indexed,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// One display title per distinct source key, fetched in a single query.
fn collect_titles(
    store: &Store,
    chunks: &[Chunk],
) -> Result<HashMap<String, String>, crate::RunError> {
    let keys: HashSet<&str> = chunks
        .iter()
        .map(|chunk| chunk.source_key.as_str())
        .collect();
    Ok(store.current_titles(&keys)?)
}

/// Outcome of one `momotaro rebuild-index` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildReport {
    /// Chunks re-indexed from canonical storage.
    pub chunks: u64,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
}

/// A search hit enriched with the source title and a text excerpt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The raw retrieval hit fields, flattened into the same JSON object.
    #[serde(flatten)]
    pub hit: RetrievalHit,
    /// Current revision title, when the source has one.
    pub title: Option<String>,
    /// Leading text of the hit chunk, ellipsized to 200 characters.
    pub excerpt: String,
}

/// The full result of one `momotaro search` invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchOutput {
    /// The query as received.
    pub query: String,
    /// Ranked hits.
    pub hits: Vec<SearchHit>,
}

/// Searches the built index; the index must exist.
///
/// Errors with [`RunError::IndexNotBuilt`] when the index directory holds
/// no tantivy `meta.json`, i.e. `momotaro index` has never run here.
pub fn search_workspace(
    root: impl AsRef<Path>,
    query: &str,
    top_k: usize,
) -> Result<SearchOutput, RunError> {
    let root = root.as_ref();
    let dir = index_dir(root);

    if !SearchIndex::exists(&dir) {
        return Err(RunError::IndexNotBuilt(dir.display().to_string()));
    }

    let search_index = SearchIndex::open_or_create(&dir)?;
    let hits = search_index.search(query, top_k)?;

    let mut store = Store::open(database_path(root))?;
    store.init_schema()?;

    let mut enriched = Vec::with_capacity(hits.len());
    for hit in hits {
        let stored = store.get_chunk(&hit.chunk_id)?;
        let excerpt = stored.map_or_else(String::new, |chunk| excerpt_of(&chunk.text));
        let title = store.current_revision_title(&hit.source_key)?;
        enriched.push(SearchHit {
            hit,
            title,
            excerpt,
        });
    }

    Ok(SearchOutput {
        query: query.to_owned(),
        hits: enriched,
    })
}

/// First 200 characters of `text`, cut on a char boundary, ellipsis when cut.
fn excerpt_of(text: &str) -> String {
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_owned();
    }
    let mut end = 0;
    for (index, _) in text.char_indices().take(EXCERPT_CHARS + 1) {
        end = index;
    }
    // `end` is the byte offset of the (EXCERPT_CHARS + 1)th character.
    format!("{}\u{2026}", &text[..end])
}
