//! Indexing, rebuilding, and searching the local vault.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use momotaro_contracts::{
    Chunk, CoverageFailure, CoverageReport, Degradation, IndexFileError, IndexFileErrorKind,
    IndexPolicy, IndexReport, OriginClass, RetrievalHit, SourceKind, WorkspaceStatus,
};
use momotaro_ingest::frontmatter::{self, FrontmatterParse};
use momotaro_ingest::{
    SourceOffset, chunk_markdown_with, derive_title, normalize_markdown, raw_name_for,
    revision_hash, source_key_for, walk_markdown_files,
};
use momotaro_retrieve::{IndexWriterHandle, RetrieveError, SearchIndex};
use momotaro_store::{NewSourceRevision, NewStoreEvent, Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

/// Reads the most recent **ingest** batch back out of the audit log (ADR 0026).
///
/// `Ok(None)` covers both "nothing has ever been written here" — no database file
/// at all, which the caller turns into "run `init`" — and "a store with no ingest
/// batch in it", which the caller turns into "run `index`". Never a batch that
/// failed, and never a fault the caller has to guess at: everything else goes
/// through the same two questions every read asks, from the same home — the
/// file-state check decides whether a file is worth opening, and `store_fault`
/// answers for it when it is not usable, so this path cannot drift from what
/// `doctor` says. It also asks for the newest *ingest* row rather than the newest
/// row, because the table carries several domains.
pub fn coverage_report(root: impl AsRef<Path>) -> Result<Option<CoverageReport>, RunError> {
    let root = root.as_ref();
    if matches!(store_file(root), StoreFile::Absent) {
        return Ok(None);
    }
    if let Some(reason) = store_fault(root) {
        return Err(RunError::StoreUnhealthy(reason));
    }
    let store = Store::open(database_path(root))?;
    let Some(latest) = store
        .latest_store_events_in(Some("ingest"), 1)?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let events = store.store_events_for_batch(&latest.batch_id)?;
    Ok(Some(summarize_coverage(&latest.batch_id, &events)))
}

/// Folds one batch's rows into the counts [`coverage_report`] reports.
///
/// A coverage row whose outcome is neither `indexed`, `skipped` nor
/// `metadata_refreshed` counts as a failure rather than being dropped: a row the
/// reader does not understand is a fact about the batch, not noise.
fn summarize_coverage(batch_id: &str, events: &[momotaro_store::StoreEvent]) -> CoverageReport {
    let mut report = CoverageReport {
        batch_id: batch_id.to_owned(),
        indexed: 0,
        skipped: 0,
        refreshed: 0,
        degradations: BTreeMap::new(),
        failures: Vec::new(),
        events: events.len() as u64,
        finished: false,
    };
    for event in events {
        match event.event_type.as_str() {
            "ingest.finished" => report.finished = true,
            "ingest.coverage" => match event.outcome.as_deref() {
                Some(outcome) if outcome == FileOutcome::INDEXED => {
                    report.indexed += 1;
                    // A degraded file is still `indexed`; the code is what says it
                    // lost something, and dropping it here is how `doctor --coverage`
                    // came to call a broken block a clean file.
                    // A code this build does not know is a fact about the batch,
                    // not noise — the same posture as an unreadable outcome below.
                    // Dropping it would report a degraded file as a clean one.
                    match event.reason_code.as_deref() {
                        None | Some("") => {}
                        Some(code) => match Degradation::from_code(code) {
                            Some(known) => *report.degradations.entry(known).or_insert(0) += 1,
                            None => report.failures.push(CoverageFailure {
                                source_key: event.source_key.clone(),
                                reason_code: event.reason_code.clone(),
                                message: Some(format!(
                                    "an indexed row carries a degradation code this build does not know: {code}"
                                )),
                            }),
                        },
                    }
                }
                Some(outcome) if outcome == FileOutcome::Skipped.as_str() => report.skipped += 1,
                Some(outcome) if outcome == FileOutcome::MetadataRefreshed.as_str() => {
                    report.refreshed += 1;
                }
                _ => report.failures.push(CoverageFailure {
                    source_key: event.source_key.clone(),
                    reason_code: event.reason_code.clone(),
                    message: event.message.clone(),
                }),
            },
            _ => {}
        }
    }
    report
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
/// Where the previous index waits while a rebuild swaps the new one in.
///
/// Only for the moment between the two renames: a successful swap discards it,
/// and a failed one renames it back.
fn retired_dir(root: &Path) -> PathBuf {
    root.join(".momotaro").join("index.retired")
}

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

    #[test]
    fn the_rebuild_joins_the_writer_and_retires_the_index_before_it_swaps() {
        // What this pins: dropping a writer is not joining it, so tantivy keeps
        // merging into the staging directory and the rename fails with `os error
        // 5` on Windows once the corpus is big enough to merge (measured: ~1.4 s
        // of busy directory; 200 notes pass, 10k fail). A true 10k end-to-end run
        // is a minutes-long, Windows-only instrument, so the *order* is the thing
        // under test: join, then retire the live index, then swap. The needles are
        // composed so this test does not count its own text.
        let source = include_str!("index_service.rs");
        let join = concat!("writer.finish()", "?;");
        let retire = concat!("fs::rename(&dir, ", "&retired)");
        let swap = concat!("fs::rename(&staging, ", "&dir)");
        let join_at = source.find(join).expect("the join call");
        let retire_at = source.find(retire).expect("the retire rename");
        let swap_at = source.find(swap).expect("the swap rename");
        assert!(
            join_at < retire_at,
            "the writer is joined before the retire"
        );
        assert!(retire_at < swap_at, "the index is retired before the swap");
        assert!(
            source.contains(concat!("discard(&", "retired)?")),
            "a successful swap unlinks the retired index"
        );
    }

    #[test]
    fn the_two_kinds_are_written_in_exactly_one_place_each() {
        // The seams pin the classification; that every store and index call is
        // routed through them cannot be driven by a test (production passes the
        // real closure), so it is counted instead. A call site that hand-writes its
        // own `map_err(.. IndexFileErrorKind::Store ..)` raises these numbers, and
        // so does a second test that names a kind — extend the count deliberately.
        let source = include_str!("index_service.rs");
        // The needle is composed rather than written, because a literal needle is
        // itself an occurrence in the file being counted (this test counted itself
        // twice before this line existed). The form `.. Store, error)` only a call
        // site has, so prose mentioning the kind cannot inflate the number either.
        let store = concat!("IndexFileErrorKind::", "Store, error)");
        let index = concat!("IndexFileErrorKind::", "Index, error)");
        assert_eq!(
            source.matches(store).count(),
            1,
            "exactly the store seam names this kind"
        );
        assert_eq!(
            source.matches(index).count(),
            1,
            "exactly the index seam names this kind"
        );
    }

    #[test]
    fn a_store_refusal_keeps_its_kind() {
        let error = store_step::<()>("note:a.md", || {
            Err(StoreError::RevisionMissing {
                source_key: "note:a.md".to_owned(),
                revision_hash: "deadbeef".to_owned(),
            })
        })
        .expect_err("a refusal is an error");
        assert_eq!(
            error.kind,
            IndexFileErrorKind::Store,
            "Store, and not Io or Other"
        );
        assert_eq!(error.source_key.as_deref(), Some("note:a.md"));
    }

    #[test]
    fn an_index_refusal_keeps_its_kind() {
        let error = index_step("note:a.md", || {
            Err(RetrieveError::SchemaMismatch("origin_class".to_owned()))
        })
        .expect_err("a refusal is an error");
        assert_eq!(
            error.kind,
            IndexFileErrorKind::Index,
            "Index, and not Store or Io"
        );
        assert!(
            error.message.contains("origin_class"),
            "the refused field passes through: {}",
            error.message
        );
    }

    #[test]
    fn the_two_steps_do_not_borrow_each_others_kind() {
        // A copy-pasted seam would map both resources onto one kind.
        let store = store_step::<()>("k", || {
            Err(StoreError::InvalidSchemaVersion("x".to_owned()))
        })
        .expect_err("a refusal is an error");
        let index = index_step("k", || Err(RetrieveError::IndexMissing("x".to_owned())))
            .expect_err("a refusal is an error");
        assert_ne!(store.kind, index.kind, "two resources, two kinds");
    }

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

/// The revision's metadata blob: the stat fingerprint plus what the frontmatter
/// block turned out to be.
fn revision_metadata(fingerprint: &str, frontmatter: &FrontmatterParse) -> String {
    let mut blob = serde_json::from_str::<serde_json::Value>(fingerprint)
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = blob.as_object_mut() {
        object.insert("frontmatter".to_owned(), frontmatter_json(frontmatter));
    }
    blob.to_string()
}

/// The block's JSON form, built here rather than in `momotaro-ingest` so the
/// parser crate stays free of a JSON dependency on its production path.
///
/// Tagged and self-describing on purpose: a reader of the blob must be able to
/// tell "there was no block" from "the block could not be read" from "here are
/// the fields" without guessing, and the failure keeps its raw text and line so
/// a later pass can retry the parse (`frontmatter_parse_error`).
/// Reads a JSON property block into the same field shape a YAML block produces.
///
/// `None` is "this build could not read it", not "it was empty". Values mirror the
/// YAML arms — a string, number or bool becomes `scalar`, an array becomes `list`,
/// anything else keeps `raw` — because a reader branching on the value shape must
/// not have to learn a second one for JSON.
fn json_frontmatter(raw: &str) -> Option<Vec<serde_json::Value>> {
    let serde_json::Value::Object(map) = serde_json::from_str::<serde_json::Value>(raw).ok()?
    else {
        return None;
    };
    Some(
        map.iter()
            .map(|(key, value)| {
                let value = match value {
                    serde_json::Value::String(text) => serde_json::json!({ "scalar": text }),
                    serde_json::Value::Number(number) => {
                        serde_json::json!({ "scalar": number.to_string() })
                    }
                    serde_json::Value::Bool(flag) => {
                        serde_json::json!({ "scalar": flag.to_string() })
                    }
                    serde_json::Value::Array(items) => {
                        let scalars = items
                            .iter()
                            .map(|item| match item {
                                serde_json::Value::String(text) => text.clone(),
                                other => other.to_string(),
                            })
                            .collect::<Vec<_>>();
                        serde_json::json!({ "list": scalars })
                    }
                    other => serde_json::json!({ "raw": other.to_string() }),
                };
                serde_json::json!({ "key": key, "value": value })
            })
            .collect(),
    )
}

fn frontmatter_json(parse: &FrontmatterParse) -> serde_json::Value {
    match parse {
        FrontmatterParse::Absent => serde_json::json!({ "state": "absent" }),
        FrontmatterParse::Parsed(frontmatter) => serde_json::json!({
            "state": "parsed",
            "raw": frontmatter.raw,
            "fields": frontmatter
                .fields
                .iter()
                .map(|field| serde_json::json!({
                    "key": field.key,
                    "value": match &field.value {
                        frontmatter::Value::Scalar(text) => serde_json::json!({ "scalar": text }),
                        frontmatter::Value::List(items) => serde_json::json!({ "list": items }),
                        frontmatter::Value::Raw(text) => serde_json::json!({ "raw": text }),
                    },
                }))
                .collect::<Vec<_>>(),
        }),
        FrontmatterParse::Json(raw) => match json_frontmatter(raw) {
            Some(fields) => serde_json::json!({
                "state": "parsed",
                "source": "json",
                "raw": raw,
                "fields": fields,
            }),
            // A JSON block this build cannot read is a block it cannot read: the
            // same state and the same reason spelling as any other unreadable
            // block, so nothing downstream learns a second vocabulary.
            None => serde_json::json!({
                "state": "failed",
                "raw": raw,
                "line": 1,
                "reason": "not_a_field",
            }),
        },
        FrontmatterParse::Failed { raw, line, reason } => serde_json::json!({
            "state": "failed",
            "raw": raw,
            "line": line,
            "reason": reason.as_str(),
        }),
    }
}

/// Appends one `ingest.coverage` row, and records the failure when there is one.
///
/// Three steps live here — bump `seq`, append the row, push the failure — and so
/// do both renderings: a refusal's `reason_code` is the typed kind's spelling and
/// its message is the very string the report carries. No call site re-types
/// either, the row cannot disagree with the report, and no clone is needed to
/// keep them equal.
// Nine arguments. The alternative a review measured is a parameter struct: +22
// lines, one fewer allow, nothing else fixed. The rule this function exists for
// is one home per rendering, not fewer parameters.
#[allow(clippy::too_many_arguments)]
fn record_coverage(
    store: &mut Store,
    report: &mut IndexReport,
    batch_id: &str,
    seq: &mut i64,
    source_key: Option<&str>,
    outcome: &str,
    reason_code: Option<&str>,
    message: Option<&str>,
    failure: Option<(IndexFileErrorKind, String)>,
) -> Result<(), RunError> {
    let refusal = failure.as_ref().map(|(kind, _)| kind.to_string());
    let reason_code = refusal.as_deref().or(reason_code);
    let message = failure.as_ref().map(|(_, text)| text.as_str()).or(message);
    *seq += 1;
    store.append_store_event(&coverage_event(
        batch_id,
        *seq,
        source_key,
        outcome,
        reason_code,
        message,
    ))?;
    if let Some((kind, message)) = failure {
        report.errors.push(file_error(source_key, kind, message));
    }
    Ok(())
}

/// What one degradation code means.
///
/// Exhaustive on purpose: a code with no sentence is a compile error, so adding
/// D41's next nine cannot silently inherit the frontmatter wording.
fn degradation_message(code: Degradation) -> &'static str {
    match code {
        Degradation::FrontmatterParseError => {
            "the file was indexed; its frontmatter block could not be read (D41.5)"
        }
        Degradation::UnresolvedWikilink => {
            "the file was indexed; a wikilink points at a note that is not here (D41.1)"
        }
        Degradation::AmbiguousWikilink => {
            "the file was indexed; a wikilink matches more than one note, so no edge was built (D41.2)"
        }
        Degradation::StaleAnchor => {
            "the file was indexed; a heading anchor does not exist in its target (D41.3)"
        }
        Degradation::MarkdownLinkDecodeError => {
            "the file was indexed; a markdown link's target could not be decoded (D41.4)"
        }
        Degradation::UnknownLinkSyntax => {
            "the file was indexed; a link uses a syntax this build does not know (D41.6)"
        }
        Degradation::CanvasParseError => {
            "the file was indexed; its canvas block could not be read as JSON (D41.8)"
        }
        Degradation::ExcalidrawDetected => {
            "the file was indexed; its Excalidraw scene stays out of the index (D41.7)"
        }
        Degradation::SymlinkEncountered => {
            "the file was indexed; a symlink under the vault was recorded, not followed (D41.9)"
        }
        Degradation::AttachmentUnindexed => {
            "the file was registered; its bytes are not searchable on purpose (D41.10)"
        }
    }
}

/// One file's store step, with the call injected.
///
/// A store refusal here is engine state a test cannot portably provoke — the file
/// is a real file, the database real and healthy — so the shape is replayed
/// rather than provoked, on the `discard_with` precedent. The kind is written once,
/// here, so the four store calls one pass makes cannot drift apart.
///
/// What this pins is the classification: which resource refused, which kind that
/// maps to, which `source_key` it is attributed to, and that the message passes
/// through unchanged. That each store call in `process_file` is routed through
/// here is not provable by a test (production passes the real closure) — the
/// and-count test below carries that half.
///
/// Still unverified, and said out loud because the instrument is silent about it:
/// the two `Io` sites and the `local_path_in` kind site have no seam of their own.
/// Moving the `path` kind to `Io` or `Other`, or either `Io` to `Other`, keeps the
/// suite green; moving an `Io` to `Store` or `Index` is caught by the and-count
/// test below, since those two spellings are watched.
fn store_step<T>(
    source_key: &str,
    step: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, IndexFileError> {
    step().map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Store, error))
}

/// One file's search-index step, with the call injected (same reason as
/// [`store_step`]; `upsert_source` is where a tantivy refusal lands).
fn index_step(
    source_key: &str,
    step: impl FnOnce() -> Result<(), RetrieveError>,
) -> Result<(), IndexFileError> {
    step().map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Index, error))
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

    // One batch per run (ADR 0026). Unique without a new dependency: the
    // millisecond is for a human reading the table, the nanosecond half makes
    // two runs in the same millisecond distinct.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let batch_id = format!("index-{}-{:x}", stamp.as_millis(), stamp.subsec_nanos());

    let mut report = IndexReport {
        batch_id: batch_id.clone(),
        files_scanned: files.len() as u64,
        files_skipped: 0,
        files_indexed: 0,
        revisions_created: 0,
        chunks_written: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };

    // The batch's own edges (ADR 0026): `started` before any file work, so a
    // reader can tell "running" from "never started"; `finished` last, after
    // every per-file row, so a batch is never called complete while a file is
    // still unrecorded.
    let mut seq: i64 = 1;
    store.append_store_event(&NewStoreEvent {
        occurred_at: now_epoch_secs(),
        batch_id: &batch_id,
        seq,
        domain: "ingest",
        event_type: "ingest.started",
        source_key: None,
        outcome: None,
        reason_code: None,
        message: None,
        data_json: &serde_json::json!({ "files_scanned": files.len() }).to_string(),
        external_id: None,
    })?;

    let mut claimed: HashMap<String, String> = HashMap::new();

    for file in files {
        let source_key = match source_key_for(&vault_root, &file) {
            Ok(key) => key,
            Err(error) => {
                // Reachable codes here: `invalid_utf8`, `outside_vault`,
                // `invalid_source_key` (see `IngestError::kind`). A non-UTF-8
                // name cannot be created on Windows, so no portable test drives
                // this arm; it stays correct by construction because the row's
                // code is the typed kind's spelling, never a literal.
                let message = error.to_string();
                record_coverage(
                    &mut store,
                    &mut report,
                    &batch_id,
                    &mut seq,
                    None,
                    "error",
                    None,
                    Some(message.as_str()),
                    Some((error.kind(), message.clone())),
                )?;
                continue;
            }
        };

        let raw_name = match raw_name_for(&vault_root, &file) {
            Ok(name) => name,
            Err(error) => {
                let message = error.to_string();
                record_coverage(
                    &mut store,
                    &mut report,
                    &batch_id,
                    &mut seq,
                    Some(source_key.as_str()),
                    "error",
                    None,
                    None,
                    Some((error.kind(), message)),
                )?;
                continue;
            }
        };

        // D42: two distinct files must never fold into one identity — NFC and
        // NFD spellings of one name, or two paths to one object. The store event
        // table exists (schema v4, ADR 0026) and this refusal is one of its rows
        // (`outcome = error`, `reason_code = key_collision`); what is still
        // missing is the disambiguating-suffix policy, so the batch refuses the
        // second claimant instead of silently overwriting the first.
        if let Some(first) = claimed.get(&source_key) {
            let message = format!("claimed by both {first:?} and {raw_name:?}");
            let kind = IndexFileErrorKind::KeyCollision;
            record_coverage(
                &mut store,
                &mut report,
                &batch_id,
                &mut seq,
                Some(source_key.as_str()),
                "error",
                None,
                None,
                Some((kind, message)),
            )?;
            continue;
        }
        claimed.insert(source_key.clone(), raw_name.clone());

        match process_file(
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
            Ok(outcome) => {
                let degraded = outcome.reason_code();
                let sentence = outcome.degradation().map(degradation_message);
                record_coverage(
                    &mut store,
                    &mut report,
                    &batch_id,
                    &mut seq,
                    Some(source_key.as_str()),
                    outcome.as_str(),
                    degraded,
                    sentence,
                    None,
                )?;
            }
            Err(IndexFileError { kind, message, .. }) => {
                record_coverage(
                    &mut store,
                    &mut report,
                    &batch_id,
                    &mut seq,
                    Some(source_key.as_str()),
                    "error",
                    None,
                    None,
                    Some((kind, message)),
                )?;
            }
        }
    }

    writer.commit()?;

    seq += 1;
    store.append_store_event(&NewStoreEvent {
        occurred_at: now_epoch_secs(),
        batch_id: &batch_id,
        seq,
        domain: "ingest",
        event_type: "ingest.finished",
        source_key: None,
        outcome: None,
        reason_code: None,
        message: None,
        data_json: &serde_json::json!({
            "files_scanned": report.files_scanned,
            "files_indexed": report.files_indexed,
            "files_skipped": report.files_skipped,
            "revisions_created": report.revisions_created,
            "chunks_written": report.chunks_written,
            "errors": report.errors.len(),
        })
        .to_string(),
        external_id: None,
    })?;

    report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(report)
}

/// One `ingest.coverage` row for one file (ADR 0026).
///
/// `outcome` is one of `indexed | skipped | metadata_refreshed | error`.
///
/// `reason_code` is set in two cases, and a reader must branch on the pair, never
/// on the outcome alone: an `error` row carries the stable spelling of the error
/// kind, and an `indexed` row carries the degradation code of something the file
/// lost on the way in (`frontmatter_parse_error`, D41.5) — the file was indexed
/// anyway, so the outcome stays `indexed`.
fn coverage_event<'a>(
    batch_id: &'a str,
    seq: i64,
    source_key: Option<&'a str>,
    outcome: &'a str,
    reason_code: Option<&'a str>,
    message: Option<&'a str>,
) -> NewStoreEvent<'a> {
    NewStoreEvent {
        occurred_at: now_epoch_secs(),
        batch_id,
        seq,
        domain: "ingest",
        event_type: "ingest.coverage",
        source_key,
        outcome: Some(outcome),
        reason_code,
        message,
        data_json: "{}",
        external_id: None,
    }
}

/// What one file's pass through [`process_file`] did (ADR 0026's outcomes).
#[derive(Debug)]
enum FileOutcome {
    /// Unchanged: same fingerprint, stored path and raw name.
    Skipped,
    /// Same bytes under a new path or spelling: the mutable columns moved.
    MetadataRefreshed,
    /// A revision and its chunks were written. `degraded` carries whatever the
    /// file lost on the way in (today: a frontmatter block that could not be
    /// read, D41.5) — the file *was* indexed, so the outcome stays `indexed` and
    /// the code rides alongside it.
    Indexed { degraded: Option<Degradation> },
}

impl FileOutcome {
    /// The wire spelling of the `indexed` outcome. A reader that only needs the
    /// word compares against this; the variant itself carries the degradation
    /// code beside the outcome.
    const INDEXED: &'static str = "indexed";

    /// The wire spelling written to `store_events.outcome` (ADR 0026).
    fn as_str(&self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::MetadataRefreshed => "metadata_refreshed",
            Self::Indexed { .. } => Self::INDEXED,
        }
    }

    /// The stable code the coverage row carries, when the file was indexed anyway.
    fn reason_code(&self) -> Option<&'static str> {
        match self {
            Self::Indexed { degraded } => degraded.map(Degradation::as_str),
            Self::Skipped | Self::MetadataRefreshed => None,
        }
    }

    /// The same fact as [`FileOutcome::reason_code`], for the callers that need
    /// the code's sentence rather than its spelling.
    fn degradation(&self) -> Option<Degradation> {
        match self {
            Self::Indexed { degraded } => *degraded,
            Self::Skipped | Self::MetadataRefreshed => None,
        }
    }
}

/// Ingests one file into the store and the search index.
///
/// `Err` carries the typed per-file failure; the caller records it, writes the
/// matching `ingest.coverage` row and moves on. Only batch-level failures
/// (config, store open, walk, commit) bubble up to [`index_workspace`].
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
) -> Result<FileOutcome, IndexFileError> {
    let metadata = fs::metadata(file)
        .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?;
    let fingerprint = fingerprint_json(&metadata);
    // Workspace-relative and `/`-joined (ADR 0024). Computed before the skip
    // check on purpose: a directory move preserves the fingerprint, so a stored
    // path may only be assumed correct if it is compared.
    let local_path = momotaro_ingest::local_path_in(workspace_root, file)
        .map_err(|error| file_error(Some(source_key), error.kind(), error))?;

    let current = store_step(source_key, || store.current_revision(source_key))?;

    if let Some(revision) = &current
        && fingerprint_matches(&revision.metadata_json, &metadata)
        && revision.local_path.as_deref() == Some(local_path.as_str())
        && revision.raw_name.as_deref() == Some(raw_name)
    {
        report.files_skipped += 1;
        return Ok(FileOutcome::Skipped);
    }

    let bytes = fs::read(file)
        .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?;
    let normalized = normalize_markdown(&bytes)
        .map_err(|error| file_error(Some(source_key), error.kind(), error))?;
    let hash = revision_hash(&normalized);

    if let Some(revision) = &current
        && revision.revision_hash == hash
    {
        // Same bytes, moved or re-spelled: refresh the mutable columns in place
        // instead of appending a revision that would say nothing new. The block is
        // re-derived from the very text that hashed equal — one blob builder, and
        // a workspace indexed before the `frontmatter` key existed gets it here.
        let (frontmatter, _) = frontmatter::split(&normalized);
        store_step(source_key, || {
            store.update_revision_fingerprint(
                source_key,
                &revision.revision_hash,
                &revision_metadata(&fingerprint, &frontmatter),
                &local_path,
                raw_name,
            )
        })?;
        report.files_indexed += 1;
        return Ok(FileOutcome::MetadataRefreshed);
    }

    let (frontmatter, body) = frontmatter::split(&normalized);
    let offset = SourceOffset::before_body(&normalized, body);
    let chunks = chunk_markdown_with(source_key, &hash, body, policy, offset);
    // The title comes from the body: a `#` inside a frontmatter block is a YAML
    // comment, not a heading.
    let title = derive_title(body, source_key);
    // The parsed block rides in the revision blob (D41.5: unknown fields are
    // Obsidian's data, so the whole block is kept rather than modelled further).
    let metadata_json = revision_metadata(&fingerprint, &frontmatter);
    // Vault content is owner content — stated once, used by the revision row and
    // the index document alike, so the two can never disagree.
    let origin = OriginClass::Owner;

    store_step(source_key, || {
        store.append_revision(&NewSourceRevision {
            source_key,
            revision_hash: &hash,
            kind: SourceKind::Note,
            title: Some(&title),
            uri: None,
            local_path: Some(&local_path),
            raw_name: Some(raw_name),
            origin_class: origin,
            metadata_json: &metadata_json,
            ingested_at: now_epoch_secs(),
        })
    })?;
    store_step(source_key, || {
        store.replace_chunks(source_key, &hash, &chunks)
    })?;

    index_step(source_key, || {
        SearchIndex::upsert_source(
            writer,
            search_index.fields(),
            source_key,
            &title,
            origin,
            &chunks,
        )
    })?;

    report.files_indexed += 1;
    report.revisions_created += 1;
    report.chunks_written += chunks.len() as u64;
    // A block that could not be read does not stop the file: the body is indexed
    // and the failure becomes a code on the coverage row (`frontmatter_parse_error`).
    let degraded = match &frontmatter {
        FrontmatterParse::Failed { .. } => Some(Degradation::FrontmatterParseError),
        // A JSON block that does not read is the same loss as a YAML block that
        // does not read, and the row says so.
        FrontmatterParse::Json(raw) if json_frontmatter(raw).is_none() => {
            Some(Degradation::FrontmatterParseError)
        }
        FrontmatterParse::Absent | FrontmatterParse::Parsed(_) | FrontmatterParse::Json(_) => None,
    };
    Ok(FileOutcome::Indexed { degraded })
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
        // Dropping the writer is not joining it: without this, the merge it was
        // still running keeps the staging directory open, and the rename below
        // fails with `os error 5` on Windows once the corpus is big enough to
        // merge (measured: 1.4 s of busy directory; 200 notes pass, 10k fail).
        writer.finish()?;
        indexed
    };
    // Retire, swap, then unlink: the index answering queries never stops
    // existing before the new one is in place, so a swap that fails leaves the
    // old index to rename back instead of a workspace with no live index at all.
    // Nothing has touched `dir` before this point — a build failing above leaves
    // it exactly as it was.
    let retired = retired_dir(root);
    if dir.exists() {
        fs::rename(&dir, &retired).map_err(|source| RunError::Io { source })?;
    }
    if let Err(source) = fs::rename(&staging, &dir) {
        // Put the live index back before reporting: the rebuild failed, and the
        // workspace should not have to.
        if !dir.exists() && retired.exists() {
            let _ = fs::rename(&retired, &dir);
        }
        return Err(RunError::Io { source });
    }
    discard(&retired)?;

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
