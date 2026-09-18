//! Indexing, rebuilding, and searching the local vault.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use momotaro_contracts::{
    Chunk, IndexFileError, IndexFileErrorKind, IndexPolicy, IndexReport, OriginClass, RetrievalHit,
    SourceKind, WorkspaceStatus,
};
use momotaro_ingest::{
    chunk_markdown, derive_title, normalize_markdown, raw_name_for, revision_hash, source_key_for,
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

/// Why the workspace's derived index cannot serve searches, when it cannot.
///
/// `None` means there is nothing wrong to report: the index is usable, or there
/// is nothing to put in it yet (a workspace that has never been indexed).
fn index_fault(root: &Path) -> Option<String> {
    let dir = index_dir(root);
    if !dir.is_dir() {
        // No index directory at all: a fault only if the store already holds
        // chunks, i.e. this workspace had an index and lost it.
        let chunks = store_chunk_count(root)?;
        return (chunks > 0)
            .then(|| format!("the index is missing while the store holds {chunks} chunks"));
    }
    if !SearchIndex::exists(&dir) {
        // A directory with no index in it: a crashed or hand-deleted index. Only
        // a fault when there is something to serve — an empty directory in a
        // workspace that was never indexed is just an empty directory.
        let chunks = store_chunk_count(root)?;
        return (chunks > 0).then(|| "index directory holds no index".to_owned());
    }
    SearchIndex::open_or_create(&dir)
        .err()
        .map(|error| error.to_string())
}

/// What the database file is, decided without opening it.
///
/// `Store::open` creates the file it does not find and switches a 0-byte file to
/// WAL, so a read path may not hand it anything it has not looked at first.
enum StoreFile {
    /// No database at all.
    Absent,
    /// The file exists with no content: a database that was never written.
    Empty,
    /// The file has content; opening it is safe.
    Present,
}

/// Classifies the database file without opening it.
fn store_file(root: &Path) -> StoreFile {
    match fs::metadata(database_path(root)) {
        Err(_) => StoreFile::Absent,
        Ok(metadata) if metadata.len() == 0 => StoreFile::Empty,
        Ok(_) => StoreFile::Present,
    }
}

/// Why the store itself is not usable, when it is not.
///
/// `None` covers both "healthy" and "no database at all": an absent store is
/// `Uninitialized`, which reads as "run `init`".
fn store_fault(root: &Path) -> Option<String> {
    match store_file(root) {
        StoreFile::Absent => None,
        // An empty file is uninitialized by definition — and saying so must not
        // open it, because opening is what writes.
        StoreFile::Empty => Some("database has not been initialized".to_owned()),
        StoreFile::Present => {
            let health = Store::open(database_path(root)).ok()?.health().ok()?;
            (health.status != WorkspaceStatus::Ready).then(|| {
                health
                    .message
                    .unwrap_or_else(|| format!("{:?}", health.status))
            })
        }
    }
}

/// Chunks in the canonical store, or `None` when the store cannot answer for
/// itself (no database file yet, empty file, uninitialized, foreign, older
/// schema) — those states have their own reports and must not be dressed up as
/// an index fault, and a read path must never be the reason a file appears.
fn store_chunk_count(root: &Path) -> Option<u64> {
    if !matches!(store_file(root), StoreFile::Present) {
        return None;
    }
    let store = Store::open(database_path(root)).ok()?;
    if store.health().ok()?.status != WorkspaceStatus::Ready {
        return None;
    }
    Some(store.counts().ok()?.chunks)
}

/// Removes a directory, tolerating the two errors that mean "it is already gone"
/// and "Windows has not finished letting go".
///
/// Only the **derived** index comes through here (the staging cleanup and the
/// swap before `rename`), so waiting is always the right answer: nothing is
/// running by then, the handle just has not closed. A canonical store is never
/// deleted.
///
/// The evidence is machine- and load-specific, and worth stating as such: on the
/// development machine, with the retry disabled, a parallel
/// `cargo test -p momotaro-run --lib` hit
/// `lose the index: Os { code: 145, DirectoryNotEmpty }` at the call site below
/// (2–6 runs in 15, measured twice); with it enabled that shape stopped
/// appearing. Other failures under load — tantivy code 5, and a lock violation
/// (33) in a test's own read — still do, and they are outside this path.
///
/// A failure names the directory it could not remove: this is the one place the
/// user is told to act on a path, so the path has to be in the message.
pub(crate) fn discard(dir: &Path) -> Result<(), RunError> {
    // A closure rather than the `fn` item: `remove_dir_all::<&Path>` is not
    // general over the argument lifetime, so passing the item itself needs a
    // higher-ranked signature it does not have.
    discard_with(dir, |dir| fs::remove_dir_all(dir)).map_err(|source| RunError::DiscardFailed {
        path: dir.to_path_buf(),
        source,
    })
}

/// The retry loop, with removal injected: the real drain shapes are timing
/// windows, so the tests replay them rather than try to provoke them.
fn discard_with(dir: &Path, remove: impl Fn(&Path) -> io::Result<()>) -> Result<(), io::Error> {
    let mut attempts = 0;
    loop {
        match remove(dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                attempts += 1;
                // 50 × 20 ms ≈ 1 s: long enough for a mapping to finish
                // unmapping, short enough to still look like a failure.
                let waiting_helps = matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::DirectoryNotEmpty
                );
                if attempts >= 50 || !waiting_helps {
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The user-facing sentence for an unusable index, remedy included.
///
/// `doctor`, `index` and `search` all report this state, so the wording — and
/// the command a user is told to run — has one source.
pub(crate) fn index_unusable_message(root: &Path) -> Option<String> {
    index_fault(root).map(|reason| format!("{reason}; run `momotaro rebuild-index` to rebuild it"))
}

/// Where a replacement index is built before it is swapped in.
pub(crate) fn staging_dir(root: &Path) -> PathBuf {
    root.join(".momotaro").join("index.rebuilding")
}

/// Where the tantivy index lives inside a workspace.
///
/// Also used by the tests that damage an index on purpose to prove the rebuild
/// path recovers it.
pub(crate) fn index_dir(root: &Path) -> PathBuf {
    root.join(".momotaro").join("index")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    /// Drives the loop with a fake remover: the real drain shapes are timing
    /// windows a test cannot reliably provoke (on this toolchain an open handle
    /// does not even block the delete), so they are replayed instead.
    fn drain_after(failures: u32, shape: io::ErrorKind) -> Cell<u32> {
        let calls = Cell::new(0);
        discard_with(Path::new("."), |_| {
            calls.set(calls.get() + 1);
            if calls.get() <= failures {
                Err(io::Error::from(shape))
            } else {
                Ok(())
            }
        })
        .expect("the retry gets there");
        calls
    }

    #[test]
    fn a_discard_failure_names_the_directory_it_tried() {
        // Drive `discard` itself, not the format string: a plain file fails
        // `remove_dir_all` on both platforms with a kind outside the retry
        // predicate, so this is deterministic and never sleeps. Replacing the
        // path with an empty `PathBuf` in the wiring has to fail here.
        let file =
            std::env::temp_dir().join(format!("momotaro-discard-is-a-file-{}", std::process::id()));
        fs::write(&file, b"x").expect("write a file");
        let error = discard(&file).expect_err("a file is not a directory");
        let RunError::DiscardFailed { path, .. } = &error else {
            panic!("expected DiscardFailed, got {error}");
        };
        assert_eq!(path, &file, "the error names the path it tried");
        let text = error.to_string();
        assert!(
            text.contains("deleting it by hand is always safe"),
            "{text}"
        );
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn a_draining_handle_is_waited_out() {
        let started = Instant::now();
        let calls = drain_after(2, io::ErrorKind::PermissionDenied);
        assert_eq!(calls.get(), 3, "two refusals, then success");
        assert!(
            started.elapsed() >= Duration::from_millis(30),
            "each retry waits: a busy loop would return instantly"
        );
    }

    #[test]
    fn a_directory_not_empty_is_a_drain_too() {
        // The shape the swap actually hit (OS 145); `ErrorKind` is what the code
        // matches on, and the portable spelling of that error.
        let calls = drain_after(2, io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_missing_directory_is_the_desired_state() {
        let calls = Cell::new(0);
        discard_with(Path::new("."), |_| {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::NotFound))
        })
        .expect("already gone is success");
        assert_eq!(calls.get(), 1, "no retry for a directory that is not there");
    }

    #[test]
    fn errors_that_waiting_cannot_fix_are_reported_at_once() {
        let calls = Cell::new(0);
        // 33 is a lock violation: not a drain, so the first failure is the answer.
        let error = discard_with(Path::new("."), |_| {
            calls.set(calls.get() + 1);
            Err(io::Error::from_raw_os_error(33))
        })
        .expect_err("a lock violation is not a drain");
        assert_eq!(calls.get(), 1);
        assert!(format!("{error}").contains("33"), "{error}");
    }

    #[test]
    fn the_retry_budget_is_bounded() {
        // A refusal that never clears must not hang: ~1 s, then the real error.
        // The wait is the point of the test, so it really waits.
        let started = Instant::now();
        let calls = Cell::new(0);
        discard_with(Path::new("."), |_| {
            calls.set(calls.get() + 1);
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .expect_err("a refusal that never clears is still a failure");
        assert_eq!(calls.get(), 50);
        assert!(started.elapsed() >= Duration::from_millis(900));
    }
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

fn file_error(
    source_key: Option<&str>,
    kind: IndexFileErrorKind,
    message: impl std::fmt::Display,
) -> IndexFileError {
    IndexFileError {
        source_key: source_key.map(str::to_owned),
        kind,
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
        // An override is an operator convenience, not a way out of the
        // workspace: ADR 0024 makes every stored path workspace-relative, so an
        // escaping override would be recorded as something that cannot round
        // trip back through `resolve_local_path`.
        Some(override_path) => {
            if !momotaro_ingest::is_within(root, override_path)? {
                return Err(RunError::VaultOutsideWorkspace {
                    vault: override_path.to_path_buf(),
                    workspace: root.to_path_buf(),
                });
            }
            override_path.to_path_buf()
        }
        None => vault_scope(root, &config)?.root,
    };
    if !vault_root.is_dir() {
        return Err(RunError::VaultMissing { path: vault_root });
    }
    // This command only adds files the store has not seen, so it cannot repair a
    // missing or unreadable index: failing here, with the command that can, beats
    // reporting "0 of 5 files indexed" while the workspace still cannot be
    // searched.
    if let Some(reason) = index_unusable_message(root) {
        return Err(RunError::IndexUnusable(reason));
    }

    // Canonicalized once per run, not once per file: path containment compares
    // canonical forms, and every scanned file needs the same root.
    let workspace_root = momotaro_ingest::canonical_workspace_root(root)?;

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

    let mut claimed: HashMap<String, String> = HashMap::new();

    for file in files {
        let source_key = match source_key_for(&vault_root, &file) {
            Ok(key) => key,
            Err(error) => {
                report.errors.push(file_error(None, error.kind(), error));
                continue;
            }
        };

        let raw_name = match raw_name_for(&vault_root, &file) {
            Ok(name) => name,
            Err(error) => {
                report
                    .errors
                    .push(file_error(Some(&source_key), error.kind(), error));
                continue;
            }
        };

        // D42: two distinct files must never fold into one identity — NFC and
        // NFD spellings of one name, or two paths to one object. Until the
        // store event table carries the collision policy (keep both entries +
        // disambiguating suffix + `key_collision`), refuse the second claimant
        // instead of silently overwriting the first.
        if let Some(first) = claimed.get(&source_key) {
            report.errors.push(file_error(
                Some(&source_key),
                IndexFileErrorKind::KeyCollision,
                format!("claimed by both {first:?} and {raw_name:?}"),
            ));
            continue;
        }
        claimed.insert(source_key.clone(), raw_name.clone());

        if let Err(message) = process_file(
            &mut store,
            &search_index,
            &mut writer,
            &workspace_root,
            &file,
            &source_key,
            &raw_name,
            policy,
            &mut report,
        ) {
            report.errors.push(file_error(
                Some(&source_key),
                IndexFileErrorKind::Other,
                message,
            ));
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
    workspace_root: &Path,
    file: &Path,
    source_key: &str,
    raw_name: &str,
    policy: &IndexPolicy,
    report: &mut IndexReport,
) -> Result<(), String> {
    let metadata = fs::metadata(file).map_err(|error| error.to_string())?;
    let fingerprint = fingerprint_json(&metadata);
    // Workspace-relative and `/`-joined (ADR 0024). Computed before the skip
    // check on purpose: a directory move preserves the fingerprint, so a stored
    // path may only be assumed correct if it is compared.
    let local_path =
        momotaro_ingest::local_path_in(workspace_root, file).map_err(|error| error.to_string())?;

    let current = store
        .current_revision(source_key)
        .map_err(|error| error.to_string())?;

    if let Some(revision) = &current
        && fingerprint_matches(&revision.metadata_json, &metadata)
        && revision.local_path.as_deref() == Some(local_path.as_str())
        && revision.raw_name.as_deref() == Some(raw_name)
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
        // Same bytes, moved or re-spelled: refresh the mutable columns in place
        // instead of appending a revision that would say nothing new.
        store
            .update_revision_fingerprint(
                source_key,
                &revision.revision_hash,
                &fingerprint,
                &local_path,
                raw_name,
            )
            .map_err(|error| error.to_string())?;
        report.files_indexed += 1;
        return Ok(());
    }

    let chunks = chunk_markdown(source_key, &hash, &normalized, policy);
    let title = derive_title(&normalized, source_key);
    // Vault content is owner content — stated once, used by the revision row and
    // the index document alike, so the two can never disagree.
    let origin = OriginClass::Owner;

    store
        .append_revision(&NewSourceRevision {
            source_key,
            revision_hash: &hash,
            kind: SourceKind::Note,
            title: Some(&title),
            uri: None,
            local_path: Some(&local_path),
            raw_name: Some(raw_name),
            origin_class: origin,
            metadata_json: &fingerprint,
            ingested_at: now_epoch_secs(),
        })
        .map_err(|error| error.to_string())?;
    store
        .replace_chunks(source_key, &hash, &chunks)
        .map_err(|error| error.to_string())?;

    SearchIndex::upsert_source(
        writer,
        search_index.fields(),
        source_key,
        &title,
        origin,
        &chunks,
    )
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
    let origins = collect_origins(&store, &chunks)?;

    let dir = index_dir(root);
    // A rebuild is a from-scratch reconstruction from canonical storage, so the
    // index directory is ours to replace; refusing to start because it is stale
    // or foreign would make the mismatch message's own advice ("rebuild the
    // index") impossible to follow. The replacement is built *beside* the live
    // index and swapped in only once it is complete: the index serving searches
    // is never left partially rebuilt or half deleted, and a failure in between
    // leaves a state `doctor` reports (a missing index while the store holds
    // chunks) instead of a silently empty one. The store is canonical either way.
    let replaced_stale = index_fault(root);
    let staging = staging_dir(root);
    if staging.exists() && !staging.is_dir() {
        // Anything else at that path is in the way, and only a human can say what
        // it was: name it rather than failing with a bare OS error.
        return Err(RunError::StagingBlocked { path: staging });
    }
    discard(&staging)?;
    let indexed = {
        fs::create_dir_all(&staging).map_err(|source| RunError::Io { source })?;
        let search_index = SearchIndex::open_or_create(&staging)?;
        let mut writer = search_index.writer(WRITER_HEAP_BYTES)?;
        let indexed = SearchIndex::rebuild_from(
            &mut writer,
            search_index.fields(),
            &chunks,
            &titles,
            &origins,
        )?;
        writer.commit()?;
        indexed
    };
    // Every handle is released above: the rename below needs the directory closed.
    // Nothing has touched `dir` up to this point — the build failing anywhere
    // above leaves the index that is answering queries exactly as it was.
    discard(&dir)?;
    fs::rename(&staging, &dir).map_err(|source| RunError::Io { source })?;

    Ok(RebuildReport {
        chunks: indexed,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        replaced_stale,
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

/// One trust class per distinct source key, fetched in a single query.
fn collect_origins(
    store: &Store,
    chunks: &[Chunk],
) -> Result<HashMap<String, OriginClass>, RunError> {
    let keys: HashSet<&str> = chunks
        .iter()
        .map(|chunk| chunk.source_key.as_str())
        .collect();
    Ok(store.current_origins(&keys)?)
}

/// Outcome of one `momotaro rebuild-index` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildReport {
    /// Chunks re-indexed from canonical storage.
    pub chunks: u64,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
    /// Why the previous index had to be thrown away, when it did (`None` when
    /// the index was already current).
    pub replaced_stale: Option<String>,
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
/// Errors with [`RunError::IndexNotBuilt`] when there is no index at all (the
/// store has no chunks to put in one), [`RunError::IndexUnusable`] when an index
/// directory exists but cannot be used, and [`RunError::StoreUnhealthy`] when the
/// store itself is the thing that is not ready.
pub fn search_workspace(
    root: impl AsRef<Path>,
    query: &str,
    top_k: usize,
) -> Result<SearchOutput, RunError> {
    let root = root.as_ref();
    let dir = index_dir(root);

    // An index directory that exists but cannot be used is not "never built": the
    // store may still hold the chunks, and only a rebuild can put them back.
    if let Some(reason) = index_unusable_message(root) {
        return Err(RunError::IndexUnusable(reason));
    }
    if !SearchIndex::exists(&dir) {
        // A store this build cannot use needs its own report, not the advice to
        // run a command that would refuse the same way.
        if let Some(reason) = store_fault(root) {
            return Err(RunError::StoreUnhealthy(reason));
        }
        return Err(RunError::IndexNotBuilt(dir.display().to_string()));
    }

    let search_index = SearchIndex::open_or_create(&dir)?;
    let hits = search_index.search(query, top_k)?;

    // The hits are citations: the store is what gives them a title and an
    // excerpt, so a query in a workspace whose database is gone has nothing to
    // cite — and this command may not create the database it would read.
    if !matches!(store_file(root), StoreFile::Present) {
        return Err(RunError::StoreUnhealthy(
            "the database is missing or empty; run `momotaro index` to rebuild it and the index it feeds"
                .to_owned(),
        ));
    }
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
