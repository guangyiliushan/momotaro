//! Indexing, rebuilding, and searching the local vault.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use momotaro_contracts::{
    AnchorKind, Chunk, CoverageCounts, CoverageFailure, CoverageReport, Degradation, GraphLink,
    IndexFileError, IndexFileErrorKind, IndexPolicy, IndexReport, OriginClass, RetrievalHit,
    SourceKind, Warning, WorkspaceStatus,
};
use momotaro_ingest::frontmatter::{self, FrontmatterParse};
use momotaro_ingest::{
    Extracted, Namespace, Outline, Resolution, SourceOffset, VaultFileKind, analyze,
    anchor_still_exists, attributes_of, chunk_markdown_with, derive_title,
    extract_frontmatter_links, is_cloud_placeholder, normalize_markdown, parent_dir,
    percent_decode, raw_name_for, resolve as resolve_link, revision_hash, revision_hash_reader,
    scene_spans, source_key_for, walk_entries, windows_incompatible_components,
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

/// Which event family one batch writes (ADR 0026).
///
/// `index` and the watcher write the `ingest` domain — a watcher batch *is* an
/// ingest batch, so `doctor --coverage` reports whichever of them wrote last —
/// and the four reconciliation triggers write `vault`. One batch loop serves
/// every caller, because the comparison, the skip rule and the projection are
/// one piece of work: a second implementation of them is how a `touch` ends up
/// reported as a new revision forever while the store holds a single row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventPlan {
    /// The `store_events.domain` these rows carry.
    pub domain: &'static str,
    /// The batch's opening event.
    pub started: &'static str,
    /// One row per file.
    pub coverage: &'static str,
    /// The batch's closing event.
    pub finished: &'static str,
    /// The prefix of this family's batch ids, so a log reader can tell them apart.
    pub prefix: &'static str,
}

/// What `index` and the watcher write.
pub(crate) const INGEST_PLAN: EventPlan = EventPlan {
    domain: "ingest",
    started: "ingest.started",
    coverage: "ingest.coverage",
    finished: "ingest.finished",
    prefix: "index",
};

/// What the watcher writes: the same family as [`INGEST_PLAN`] — a watcher
/// batch *is* an ingest batch, and `doctor --coverage` reads whichever ran
/// last — under its own id prefix, so a reader of `store_events` can still
/// tell who wrote it.
pub(crate) const WATCH_PLAN: EventPlan = EventPlan {
    prefix: "watch",
    ..INGEST_PLAN
};

/// What the four reconciliation triggers write (D40 §4.4).
pub(crate) const RECONCILE_PLAN: EventPlan = EventPlan {
    domain: "vault",
    started: "vault.reconcile.started",
    coverage: "vault.reconcile.coverage",
    finished: "vault.reconcile.finished",
    prefix: "reconcile",
};

/// One batch's identity, its family, and how far into it we are.
///
/// Carried as a single value rather than as `batch_id` + `seq` parameters so
/// the plan travels with the id: a row with the right sequence and the wrong
/// family is precisely the drift this type exists to make impossible.
#[derive(Debug)]
pub(crate) struct Batch {
    /// The id every row of this run carries.
    pub id: String,
    /// The family these rows belong to.
    pub plan: EventPlan,
    /// The last sequence handed out; `0` before the first row.
    seq: i64,
}

impl Batch {
    /// Opens a batch whose id reads as what it is.
    pub fn new(plan: EventPlan) -> Self {
        // Unique without a new dependency: the millisecond is for a human
        // reading the table, the nanosecond half separates two batches inside
        // the same millisecond.
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            id: format!(
                "{}-{}-{:x}",
                plan.prefix,
                stamp.as_millis(),
                stamp.subsec_nanos()
            ),
            plan,
            seq: 0,
        }
    }

    /// The next sequence number in this batch.
    pub fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }
}

/// What kind of batch this run is (D40 §4.4 / §4.7).
#[derive(Debug, Clone)]
pub(crate) struct BatchShape {
    /// The event family these rows belong to.
    pub plan: EventPlan,
    /// The files to process. `None` is every file the walk found — `index` and
    /// reconcile. `Some` is a subset, and the rest of the walk is still there
    /// for the namespace, the prune and the deletion check: that is how one
    /// watcher batch costs a single file while its links resolve against the
    /// whole vault.
    pub only: Option<HashSet<PathBuf>>,
    /// Gate every path before touching it (D40 §4.2 / §4.3) and put each
    /// rebuilt file's row inside its own transaction (D40 §4.7).
    pub gated: bool,
}

impl BatchShape {
    /// A batch over the whole walk that takes the walk's word for what changed.
    pub fn full(plan: EventPlan) -> Self {
        Self {
            plan,
            only: None,
            gated: false,
        }
    }

    /// The watcher's batch: these paths only, each one gated before it is read.
    pub fn watched(plan: EventPlan, only: HashSet<PathBuf>) -> Self {
        Self {
            plan,
            only: Some(only),
            gated: true,
        }
    }
}

/// What one batch did, including what it could not finish.
#[derive(Debug)]
pub(crate) struct BatchOutcome {
    /// What the caller renders.
    pub report: IndexReport,
    /// Keys the walk no longer names, whatever the caller asked for — the
    /// deletion check runs on every batch because the walk runs on every batch
    /// (D40 §4.2).
    pub removed: u64,
    /// Paths to hand back to the watcher's retry queue: torn reads (D40 §4.5)
    /// and guard refusals (D40 §4.7). The batch recorded an error row for a
    /// torn read and nothing at all for a refusal — either way the watcher
    /// backs off, then gives up to the daily reconcile.
    pub retry: Vec<PathBuf>,
}

/// What the cheap pass concluded about one path (D40 §4.2 / §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The path holds no file any more.
    Absent,
    /// The path holds a directory.
    ///
    /// A directory rename or `mkdir` produces events for the *directory*, never
    /// for the files inside it, so the children of the old path are about to
    /// look absent from the walk — and the children of the new path have had no
    /// event at all. The caller answers by rescanning everything under it.
    Directory,
    /// The stat matches and is not racy: a false event, dropped with no read
    /// beyond the stat and with no row.
    Drop,
    /// The stat matches but the bytes do not: the skip rule would swallow a
    /// real change, so the rebuild path has to be forced.
    ForceRebuild,
    /// There is work — new bytes, a moved path, a changed fingerprint, or a
    /// racy file whose bytes matched and so earns its row (D40 §4.3).
    Proceed,
}

/// The watcher's cheap pass over one path, before anything is written.
///
/// Deliberately separate from `process_file`: this decides *whether* to do the
/// work, and reaches for content only when the stat cannot answer (a racy one).
/// A path that passes here is read for real, and the guard inside its
/// transaction re-checks the stat — so nothing decided here has to be trusted.
pub(crate) fn gate(
    store: &Store,
    workspace_root: &Path,
    vault_root: &Path,
    file: &Path,
    coarse: bool,
    last_written: i64,
) -> Result<Gate, RunError> {
    let metadata = match fs::metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Gate::Absent),
        Err(error) => return Err(RunError::Io { source: error }),
    };
    // The walker only ever yields files, so a path that is not one is a
    // directory question, not a file question: see [`Gate::Directory`].
    if !metadata.is_file() {
        return Ok(Gate::Directory);
    }
    let source_key = source_key_for(vault_root, file)?;
    let raw_name = raw_name_for(vault_root, file)?;
    let local_path = momotaro_ingest::local_path_in(workspace_root, file)?;
    let Some(current) = store.current_revision(&source_key)? else {
        return Ok(Gate::Proceed);
    };
    let stat_matches = fingerprint_matches(&current.metadata_json, &metadata)
        && current.local_path.as_deref() == Some(local_path.as_str())
        && current.raw_name.as_deref() == Some(raw_name.as_str());
    if !stat_matches {
        return Ok(verdict(false, false, None));
    }
    // D40 §4.3: a coarse filesystem (FAT/exFAT, two-second ticks) can leave a
    // rewritten file with a stat that still matches, so the shortcut is not
    // available there either. `gate` only ever runs because an event arrived,
    // which is exactly the clause's "最近 N 秒内有事件"; the label and the store
    // write moment are both answers about the *batch*, so the caller reads
    // them once instead of three Win32 calls and a query per path.
    if !is_racy(mtime_secs(&metadata), last_written) && !coarse {
        return Ok(verdict(true, false, None));
    }
    // The stat proves nothing here, so compare the bytes (D40 §4.3).
    let hash_matches = match read_stable(file).map_err(|source| RunError::Io { source })? {
        // Let the real pass own the retry: it is the one that writes.
        StableRead::Torn => None,
        StableRead::Stable(bytes) => {
            let normalized = normalize_markdown(&bytes)?;
            Some(revision_hash(&normalized) == current.revision_hash)
        }
    };
    // Same bytes: the event was false, but this file is racy, so it earns its
    // `skipped` row — and that row is what advances `recorded_at` past the
    // file's mtime and clears the raciness (D40 §4.3, git's `--refresh`).
    Ok(verdict(true, true, hash_matches))
}

/// How far a file's pass got, for the two callers that answer it differently.
///
/// A torn read is not an `IndexFileErrorKind`: it is not a classification of
/// *what* failed but a statement that the bytes were never one version, and
/// the batch records it as an I/O row while the watcher backs off and
/// requeues (D40 §4.5). Typing it keeps the watcher from having to match on a
/// message, which this repo forbids.
enum PassError {
    /// The file changed while it was read; nothing was written.
    Torn,
    /// The transaction's first step refused the unit (D40 §4.7): the whole
    /// unit rolled back and the caller requeues rather than records a
    /// failure — nothing was written, so nothing failed.
    Guard,
    /// A failure already shaped for a row.
    File(IndexFileError),
    /// A failure the per-file path cannot classify into a row — the store
    /// refused a read that the row itself does not describe.
    Run(RunError),
}

impl From<IndexFileError> for PassError {
    fn from(error: IndexFileError) -> Self {
        Self::File(error)
    }
}

impl From<RunError> for PassError {
    fn from(error: RunError) -> Self {
        Self::Run(error)
    }
}

impl From<StoreError> for PassError {
    fn from(error: StoreError) -> Self {
        Self::Run(RunError::Store(error))
    }
}

/// A store step that has to keep the guard's refusal distinguishable.
///
/// `store_step` classifies every store failure as `store`, which is right for
/// the reads — but §4.7 says a guard rejection *requeues*, and a row that says
/// "store error" would say the opposite of what happened: the unit rolled
/// back and nothing was attempted. The classification stays identical for
/// every other failure; only the refusal keeps its own variant.
fn commit_step<T>(
    source_key: &str,
    step: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, PassError> {
    step().map_err(|error| {
        let kind = IndexFileErrorKind::Store;
        match error {
            StoreError::GuardRejected(_) => PassError::Guard,
            other => PassError::File(file_error(Some(source_key), kind, other)),
        }
    })
}

/// Whether the caller insists on the content path although the stat matches
/// (D40 §4.3): the stat was racy and the bytes differ.
fn forced(mode: &ProcessMode<'_>) -> bool {
    matches!(
        mode,
        ProcessMode::Watch {
            force_content: true,
            ..
        }
    )
}

/// The transaction's first step: re-check the stat the bytes were read under
/// (D40 §4.7).
///
/// A refusal rolls the whole unit back and the caller requeues — the file
/// moved between our read and our write, so what we hold describes nothing
/// that is there. Deliberately no path in the message: it reaches a terminal.
fn guard_against(file: &Path, expected: &fs::Metadata) -> impl FnOnce() -> Result<(), StoreError> {
    let expected_mtime = mtime_ms(expected);
    let expected_len = expected.len();
    let file = file.to_owned();
    move || match fs::metadata(&file) {
        Ok(stat) if mtime_ms(&stat) == expected_mtime && stat.len() == expected_len => Ok(()),
        _ => Err(StoreError::GuardRejected(GUARD_REFUSED.to_owned())),
    }
}

/// One file's read with a stat on each side (D40 §4.2).
enum StableRead {
    /// The bytes. The two stats matched, so the caller needs neither of them —
    /// the guard re-reads the file at commit time (D40 §4.7).
    Stable(Vec<u8>),
    /// The file changed between the two stats, so the buffer holds two
    /// versions of it. It is dropped here and never handed on.
    Torn,
}

/// Read `path`, checking its stat before and after.
///
/// No retry lives here: the batch answers a torn read with an error row and
/// the watcher with a backed-off requeue, and that choice belongs to the
/// caller. What this guarantees is the contract's absolute half — torn bytes
/// never reach a caller that would commit them.
fn read_stable(path: &Path) -> io::Result<StableRead> {
    let before = fs::metadata(path)?;
    let bytes = fs::read(path)?;
    let after = fs::metadata(path)?;
    if mtime_ms(&before) != mtime_ms(&after) || before.len() != after.len() {
        return Ok(StableRead::Torn);
    }
    let _ = after;
    Ok(StableRead::Stable(bytes))
}

/// Whether a matching stat can be trusted for this file (D40 §4.3).
///
/// git's rule with git's reference: an entry is racily clean when its mtime is
/// at or after the moment the index file itself was last written, because a
/// file rewritten in that same tick keeps a stat that matches while its bytes
/// differ. The `>=` is git's, and it is the whole test: `last_written` comes
/// from [`last_written`], and a store that has never been written cannot vouch
/// for anything — `0` makes everything racy, which is the honest answer.
fn is_racy(mtime_secs: i64, last_written: i64) -> bool {
    mtime_secs >= last_written
}

/// The moment the store was last written — the boundary [`is_racy`] measures
/// against (D40 §4.3).
///
/// Read once per batch: nothing inside a gate loop writes to the store, so the
/// answer cannot change under it, and reading it per path costs a query to
/// fetch one `i64`.
pub(crate) fn last_written(store: &Store) -> i64 {
    store
        .latest_store_events(1)
        .ok()
        .and_then(|rows| rows.first().map(|row| row.recorded_at))
        .unwrap_or(0)
}

/// A stat's mtime in whole seconds — the unit git's rule and ours both use.
fn mtime_secs(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs() as i64)
}

/// The verdict on one file, given what the cheap pass found (D40 §4.2 / §4.3).
///
/// Split out from [`gate`] so every combination can be pinned without a
/// machine that can preserve an mtime while changing a file's bytes — which no
/// portable test can build.
///
/// * stat differs, or there is no row: there is work.
/// * stat matches and is not racy: a false event, dropped with no row.
/// * stat matches, is racy, and the bytes agree: still no work, but the file
///   earns the row that clears its raciness (D40 §4.3).
/// * stat matches, is racy, and the bytes differ: the skip rule would swallow
///   a real change, so the rebuild path is forced.
/// * the read tore: hand it to the pass that owns the retry (D40 §4.5).
fn verdict(stat_matches: bool, racy: bool, hash_matches: Option<bool>) -> Gate {
    if !stat_matches {
        return Gate::Proceed;
    }
    if !racy {
        return Gate::Drop;
    }
    match hash_matches {
        Some(true) => Gate::Proceed,
        Some(false) => Gate::ForceRebuild,
        None => Gate::Proceed,
    }
}

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
    // Anchored on the batch's opener: a `source_moved` hint shares this domain
    // and has no shell, so "the newest row" is not "the last ingest run".
    let Some(batch_id) = store.latest_batch_opened_in("ingest", "ingest.started")? else {
        return Ok(None);
    };
    let events = store.store_events_for_batch(&batch_id)?;
    Ok(Some(summarize_coverage(&batch_id, &events)))
}

/// Counts one row's codes, from its whole lists when it carries them and from
/// its primary code otherwise.
///
/// The fallback is for rows written before the lists existed (and for the
/// refusal rows, which never carry one), and it consults **both** vocabularies:
/// `reason_code` holds the first degradation, or the first warning on a row
/// with no degradation (§5 第 8 条 ⑤), and a reader that tried only one
/// vocabulary would report a warning as a failure. A code neither vocabulary
/// knows is a fact about the batch, not noise — the same posture as an
/// unreadable outcome below.
fn count_codes(report: &mut CoverageReport, event: &momotaro_store::StoreEvent) {
    let value = serde_json::from_str::<serde_json::Value>(&event.data_json).ok();
    let list = |key: &str| -> Vec<String> {
        value
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(|list| serde_json::from_value::<Vec<String>>(list.clone()).ok())
            .unwrap_or_default()
    };
    let mut degradation_codes = list("degradations");
    let mut warning_codes = list("warnings");
    if degradation_codes.is_empty()
        && warning_codes.is_empty()
        && let Some(code) = event.reason_code.as_deref()
    {
        if Warning::from_code(code).is_some() {
            warning_codes.push(code.to_owned());
        } else {
            degradation_codes.push(code.to_owned());
        }
    }
    // The axis is the row's, not the code's: the same code rides rows that are in
    // the index anyway and rows nothing was read for.
    let in_index = matches!(
        event.outcome.as_deref(),
        Some(FileOutcome::INDEXED) | Some(FileOutcome::METADATA_REFRESHED)
    );
    for code in &degradation_codes {
        match Degradation::from_code(code) {
            Some(known) => count_row(&mut report.degradations, known, in_index),
            None => report.failures.push(CoverageFailure {
                source_key: event.source_key.clone(),
                reason_code: Some(code.clone()),
                message: Some(format!(
                    "a row carries a degradation code this build does not know: {code}"
                )),
            }),
        }
    }
    for code in &warning_codes {
        match Warning::from_code(code) {
            Some(known) => count_row(&mut report.warnings, known, in_index),
            None => report.failures.push(CoverageFailure {
                source_key: event.source_key.clone(),
                reason_code: Some(code.clone()),
                message: Some(format!(
                    "a row carries a warning code this build does not know: {code}"
                )),
            }),
        }
    }
}

/// Adds one row's verdict to a code's counts: `in_index` means the row went
/// into the index anyway, and anything else means nothing was read for it.
fn count_row<T: Ord>(map: &mut BTreeMap<T, CoverageCounts>, code: T, in_index: bool) {
    let counts = map.entry(code).or_default();
    if in_index {
        counts.in_index += 1;
    } else {
        counts.skipped += 1;
    }
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
        warnings: BTreeMap::new(),
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
                    count_codes(&mut report, event);
                }
                Some(outcome) if outcome == FileOutcome::SKIPPED => {
                    report.skipped += 1;
                    // A skipped row can carry a code too — a link the index does not
                    // follow says so on its row, and a reader that drops it would
                    // report the batch as having lost nothing.
                    count_codes(&mut report, event);
                }
                Some(outcome) if outcome == FileOutcome::METADATA_REFRESHED => {
                    report.refreshed += 1;
                    // A refreshed file lost whatever the re-derived block says it
                    // lost: this path used to keep the blob's answer to itself.
                    count_codes(&mut report, event);
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
    fn the_row_payload_carries_each_list_only_when_it_has_one() {
        // The shape the coverage row's `data_json` carries, pinned on the pure
        // function that builds it — the empty payload is exactly `{}`, each list
        // is written only when it is non-empty, and a row can carry both
        // vocabularies at once (§5 第 8 条).
        let empty = RowPayload {
            degradations: &[],
            warnings: &[],
            windows_names: &[],
            ambiguous: &[],
            stale: &[],
        };
        assert_eq!(payload_json(&empty), "{}");
        // The constant a refusal row carries is that very answer, spelled once
        // so no caller can drift from it.
        assert_eq!(EMPTY_PAYLOAD, payload_json(&empty));
        assert_eq!(
            payload_json(&RowPayload {
                degradations: &[
                    Degradation::FrontmatterParseError,
                    Degradation::ExcalidrawDetected,
                ],
                warnings: &[],
                windows_names: &[],
                ambiguous: &[],
                stale: &[],
            }),
            "{\"degradations\":[\"frontmatter_parse_error\",\"excalidraw_detected\"]}"
        );
        assert_eq!(
            payload_json(&RowPayload {
                degradations: &[Degradation::FrontmatterParseError],
                warnings: &[Warning::WindowsIncompatibleName],
                windows_names: &["dir.".to_owned()],
                ambiguous: &[],
                stale: &[],
            }),
            "{\"degradations\":[\"frontmatter_parse_error\"],\"warnings\":[\"windows_incompatible_name\"],\"windows_incompatible_names\":[\"dir.\"]}"
        );
        assert_eq!(
            payload_json(&RowPayload {
                degradations: &[],
                warnings: &[Warning::IcloudPlaceholder],
                windows_names: &[],
                ambiguous: &[],
                stale: &[],
            }),
            "{\"warnings\":[\"icloud_placeholder\"]}"
        );
        // An ambiguous link carries its writing and its candidates (§5 第 3 条):
        // the audit of a no-edge decision.
        assert_eq!(
            payload_json(&RowPayload {
                degradations: &[Degradation::AmbiguousWikilink],
                warnings: &[],
                windows_names: &[],
                ambiguous: &[(
                    "note".to_owned(),
                    vec!["note:a/note.md".to_owned(), "note:b/note.md".to_owned()]
                )],
                stale: &[],
            }),
            // serde_json maps sort their keys, so the payload's key order is
            // alphabetical and deterministic — pinned here as written.
            "{\"ambiguous\":[{\"candidates\":[\"note:a/note.md\",\"note:b/note.md\"],\"raw_target\":\"note\"}],\"degradations\":[\"ambiguous_wikilink\"]}"
        );
        // A stale anchor carries which one (target + raw), like the ambiguous
        // candidates do — the audit of a kept-but-unverified edge (§5 第 4 条).
        assert_eq!(
            payload_json(&RowPayload {
                degradations: &[Degradation::StaleAnchor],
                warnings: &[],
                windows_names: &[],
                ambiguous: &[],
                stale: &[("note:target.md".to_owned(), "NoSuch".to_owned())],
            }),
            "{\"degradations\":[\"stale_anchor\"],\"stale_anchors\":[{\"anchor\":\"NoSuch\",\"target\":\"note:target.md\"}]}"
        );
    }

    #[test]
    fn a_blob_record_from_a_newer_build_is_skipped_not_rewritten() {
        let blob = serde_json::json!({
            "links": [
                {"raw_target": "x", "edge_kind": "kind_from_the_future",
                 "link_source": "wikilink", "anchor_kind": null, "anchor_raw": null},
                {"raw_target": "y", "edge_kind": "link", "link_source": "wikilink",
                 "anchor_kind": "anchor_from_the_future", "anchor_raw": "H"},
                {"raw_target": "z", "edge_kind": "link", "link_source": "source_from_the_future",
                 "anchor_kind": null, "anchor_raw": null},
            ],
            "link_losses": [],
            "outline": {"headings": [], "block_ids": []},
        })
        .to_string();
        let out = parse_link_blob(&blob);
        assert!(
            out.links.is_empty(),
            "an unknown spelling anywhere drops the record whole: {:?}",
            out.links
        );
        assert_eq!(
            out.losses,
            [Degradation::UnknownLinkSyntax],
            "and the drop is reported, never silent (§5 第 7 条)"
        );
    }

    #[test]
    fn the_links_blob_record_is_the_contract_shape() {
        // The persisted record is contract (§5 第 2 条): exact keys, exact
        // spellings, nulls where the anchor is absent — asserted against the
        // literal JSON so a drift cannot agree with itself.
        let input = chunk_input("body [[x]]", VaultFileKind::Note);
        let blob = revision_metadata("{}", &input.frontmatter, &input.extracted, &input.outline);
        let value: serde_json::Value = serde_json::from_str(&blob).expect("blob is JSON");
        assert_eq!(
            value["links"][0],
            serde_json::json!({
                "raw_target": "x",
                "edge_kind": "link",
                "link_source": "wikilink",
                "anchor_kind": null,
                "anchor_raw": null,
            }),
            "{blob}"
        );
        assert_eq!(value["link_losses"], serde_json::json!([]), "{blob}");
        assert!(
            value.get("outline").is_some(),
            "the outline key is always there"
        );
    }

    #[test]
    fn every_code_carries_the_sentence_this_table_pins() {
        // The two match blocks below are written by hand, and what a row's
        // reader sees is their output — a review's probe swapped two sentences
        // and no assertion noticed. The whole table is pinned here: a swap
        // fails, and the enums' exhaustiveness keeps a new code from having no
        // sentence at all.
        for (code, sentence) in [
            (
                Degradation::FrontmatterParseError,
                "the file was indexed; its frontmatter block could not be read (D41.5)",
            ),
            (
                Degradation::UnresolvedWikilink,
                "the file was indexed; a wikilink points at a note that is not here (D41.1)",
            ),
            (
                Degradation::AmbiguousWikilink,
                "the file was indexed; a wikilink matches more than one note, so no edge was built (D41.2)",
            ),
            (
                Degradation::StaleAnchor,
                "the file was indexed; a heading anchor does not exist in its target (D41.3)",
            ),
            (
                Degradation::MarkdownLinkDecodeError,
                "the file was indexed; a markdown link's target could not be decoded (D41.4)",
            ),
            (
                Degradation::UnknownLinkSyntax,
                "the file was indexed; a link uses a syntax this build does not know (D41.6)",
            ),
            (
                Degradation::CanvasParseError,
                "the file was indexed; its canvas block could not be read as JSON (D41.8)",
            ),
            (
                Degradation::ExcalidrawDetected,
                "the file was indexed; its Excalidraw scene stays out of the index (D41.7)",
            ),
            (
                Degradation::SymlinkEncountered,
                "the path is a symlink; it was recorded, not followed (D41.9)",
            ),
            (
                Degradation::AttachmentUnindexed,
                "the file was registered; its bytes are not searchable on purpose (D41.10)",
            ),
        ] {
            assert_eq!(
                degradation_message(code),
                sentence,
                "{code:?} must carry the sentence its row shows"
            );
        }
        for (warning, sentence) in [
            (
                Warning::OneDrivePlaceholder,
                "the file is a cloud placeholder; its bytes are not on this device, so nothing was read (D42)",
            ),
            (
                Warning::IcloudPlaceholder,
                "the file is an iCloud eviction stub; the note it stands for is not on this device (D42)",
            ),
            (
                Warning::WindowsIncompatibleName,
                "the name is one Windows' naming rules reserve; tools and sync clients that follow them will refuse it (D42)",
            ),
        ] {
            assert_eq!(warning_message(warning), sentence, "{warning:?}");
        }
    }

    #[test]
    fn a_canvas_becomes_exactly_the_text_its_fields_spell_out() {
        // One literal, so the whole rendering is pinned: which fields become text,
        // that a `group` with no `label` contributes no segment, that a node type
        // this build does not know contributes none, and that the order is the
        // arrays' own (in JSON Canvas, array order is the z-index).
        // `contains` assertions cannot see a placeholder or a reordering — a
        // reviewer measured that both survive them; this cannot.
        let canvas = r##"{
  "nodes": [
    {"id": "8f1a", "type": "text", "text": "alphatoken card", "x": 1, "y": 2, "width": 3, "height": 4, "color": "4"},
    {"id": "1122", "type": "file", "file": "readme.md", "subpath": "#Headingtoken"},
    {"id": "99aa", "type": "link", "url": "https://example.com/linktoken"},
    {"id": "0011", "type": "group", "label": "grouptoken"},
    {"id": "dead", "type": "group"},
    {"id": "cafe", "type": "no-such-kind", "text": "shouldnotappear"}
  ],
  "edges": [
    {"id": "e1", "fromNode": "8f1a", "toNode": "1122", "label": "edgelabel"},
    {"id": "e2", "fromNode": "8f1a", "toNode": "not-an-id"}
  ]
}"##;
        assert_eq!(
            canvas_text(canvas).expect("a readable canvas"),
            "alphatoken card\n\nreadme.md#Headingtoken\n\nhttps://example.com/linktoken\n\ngrouptoken\n\nalphatoken card -> readme.md#Headingtoken: edgelabel\n\nalphatoken card -> not-an-id",
            "the segments, in order, and nothing invented"
        );
    }

    #[test]
    fn a_canvas_that_does_not_read_as_an_object_is_the_failure_case() {
        // The spec has no rules for unreadable JSON, so the rule is ours (§5.9):
        // text that does not read as a canvas object is kept verbatim and reported,
        // rather than quietly indexed as JSON noise. An empty object is not that
        // case — it is a canvas with nothing in it.
        assert!(canvas_text("[1,2,3]").is_none());
        assert!(canvas_text("null").is_none());
        assert!(canvas_text("not json at all").is_none());
        // Present but not an array: the file claims to be a canvas and is not one,
        // so it must not come out looking like an empty canvas.
        assert!(canvas_text("{\"nodes\": 5}").is_none());
        assert!(canvas_text("{\"nodes\": null}").is_none());
        assert!(canvas_text("{\"edges\": \"x\"}").is_none());
        assert_eq!(
            canvas_text("{}"),
            Some(String::new()),
            "an empty canvas is empty, not failed"
        );
    }

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
    fn every_ingest_write_goes_through_the_unit() {
        // The wiring cannot be driven through the public API (production passes
        // the real store), so it is counted, on the precedent just above:
        // reverting a call site to `append_revision` + `replace_chunks` must go
        // red here, or "one file is one transaction" quietly degrades to "the
        // API exists". The needle is split so this test is not its own hit.
        let source = include_str!("index_service.rs");
        let calls = concat!("store.commit_source", "_file(");
        assert_eq!(
            source.matches(calls).count(),
            2,
            "both ingest write sites commit through the unit"
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

    fn store_with_one_event() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("init schema");
        store
            .append_store_event(&NewStoreEvent {
                occurred_at: 1_000,
                batch_id: "index-1",
                seq: 1,
                domain: "ingest",
                event_type: "ingest.started",
                source_key: None,
                outcome: None,
                reason_code: None,
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append");
        (dir, store)
    }

    #[test]
    fn racy_is_at_or_after_the_last_write_not_after_it() {
        // git's boundary, with git's direction: an entry whose mtime equals the
        // index's own write time is racily clean, because a file rewritten in
        // that same tick keeps a stat that matches. Writing `<` instead of
        // `<=` here would let exactly that file through the cheap pass.
        let written = 1_000;
        assert!(is_racy(written, written), "equal is racy");
        assert!(is_racy(written + 1, written), "later is racy");
        assert!(
            !is_racy(written - 1, written),
            "written before the last write is settled"
        );
    }

    #[test]
    fn the_boundary_is_read_from_the_store_and_not_invented() {
        // The pure rule is only half of it: the boundary has to come from the
        // log's newest `recorded_at`, and a log with no write in it has to say
        // "never" rather than pick a timestamp — because a boundary nothing
        // wrote would let the cheap pass vouch for a match nobody made.
        let (_dir, store) = store_with_one_event();
        let recorded = store.latest_store_events(1).expect("read")[0].recorded_at;
        assert!(recorded > 0, "the fixture wrote a row");
        assert_eq!(
            last_written(&store),
            recorded,
            "the newest row is the boundary"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let fresh = Store::open(dir.path().join("momotaro.db")).expect("open store");
        assert_eq!(last_written(&fresh), 0, "an empty log has no write moment");
        assert!(
            is_racy(1, last_written(&fresh)),
            "an empty log cannot vouch for a match, so everything is racy"
        );
        assert!(
            is_racy(0, last_written(&fresh)),
            "not even the epoch is settled by it"
        );
    }

    #[test]
    fn only_a_watcher_writes_the_row_it_was_asked_for() {
        // ADR 0026 / D40 §4.7: under the watcher the row lands now, under a
        // batch it does not — the batch writes it at its tail. Both halves
        // have to be asserted, because either one alone passes with the other
        // broken.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open");
        store.init_schema().expect("schema");
        let empty_report = || IndexReport {
            batch_id: String::new(),
            files_skipped: 0,
            files_indexed: 0,
            revisions_created: 0,
            chunks_written: 0,
            errors: Vec::new(),
            duration_ms: 0,
            files_scanned: 0,
        };
        let row = |key: &str| PendingRow {
            source_key: Some(key.to_owned()),
            raw_name: Some("a.md".to_owned()),
            outcome: Ok(FileOutcome::Skipped {
                degradations: Vec::new(),
                warnings: Vec::new(),
            }),
            row_written: false,
            replace_rows: None,
            anchors: Vec::new(),
            ambiguous: Vec::new(),
        };

        let mut report = empty_report();
        let mut batch = Batch::new(WATCH_PLAN);
        let mut mode = ProcessMode::Watch {
            force_content: false,
            batch: &mut batch,
        };
        row_now(
            &mut store,
            &mut report,
            &mut mode,
            row("note:watched"),
            Vec::new(),
        )
        .expect("watch row");
        assert_eq!(
            store.latest_store_events(10).expect("read").len(),
            1,
            "a watcher's row is written by this call"
        );

        let mut report = empty_report();
        let mut mode = ProcessMode::Batch;
        row_now(
            &mut store,
            &mut report,
            &mut mode,
            row("note:batched"),
            Vec::new(),
        )
        .expect("batch row is a no-op");
        assert_eq!(
            store.latest_store_events(10).expect("read").len(),
            1,
            "a batch's row is written at its tail, never here"
        );
    }

    #[test]
    fn a_failed_watched_file_gets_exactly_one_row() {
        // ADR 0026: one `ingest.coverage` row per file per batch. Under the
        // watcher both `process_file` and `pass_one` used to write the refusal
        // — two rows and a double-counted error — so this drives a real
        // failure through `pass_one` and counts what landed.
        //
        // The failure is a directory wearing a `.md` name: `fs::metadata`
        // happily stats it and `fs::read` refuses, which is a per-file I/O
        // failure on exactly the path a watcher would be handed.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("not-a-note.md")).expect("mkdir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open");
        store.init_schema().expect("schema");
        let index = SearchIndex::open_or_create(&dir.path().join("index")).expect("index");
        let mut writer = index.writer(WRITER_HEAP_BYTES).expect("writer");
        let policy = IndexPolicy::default();
        let ns = Namespace::new(std::iter::empty());
        let non_note: HashSet<String> = HashSet::new();
        let ctx = PassContext {
            vault_root: &vault,
            workspace_root: dir.path(),
            policy: &policy,
            ns: &ns,
            non_note_keys: &non_note,
            attributes_of: &|_| Ok(0),
            search_index: &index,
        };
        let mut claimed: HashMap<String, String> = HashMap::new();
        let mut report = IndexReport {
            batch_id: String::new(),
            files_scanned: 1,
            files_skipped: 0,
            files_indexed: 0,
            revisions_created: 0,
            chunks_written: 0,
            errors: Vec::new(),
            duration_ms: 0,
        };
        let mut batch = Batch::new(WATCH_PLAN);
        let mode = ProcessMode::Watch {
            force_content: false,
            batch: &mut batch,
        };

        let passed = pass_one(
            VaultFileKind::Note,
            &vault.join("not-a-note.md"),
            &ctx,
            &mut claimed,
            &mut store,
            &mut writer,
            &mut report,
            mode,
        );
        let PassOne::Failed { .. } = passed.expect("a read failure is a refusal") else {
            panic!("the file could not be read, so it is a refusal")
        };

        let rows: Vec<_> = store
            .store_events_for_batch(&batch.id)
            .expect("read batch")
            .into_iter()
            .filter(|event| event.event_type == "ingest.coverage")
            .collect();
        assert_eq!(rows.len(), 1, "one refusal, one row — not two");
        assert_eq!(rows[0].outcome.as_deref(), Some("error"));
        assert_eq!(
            report.errors.len(),
            1,
            "and the batch counted it exactly once"
        );
    }

    #[test]
    fn the_guard_refuses_when_the_file_moves_under_the_commit() {
        // D40 §4.7: the guard is the transaction's first step, so it has to be
        // able to say no. The store's own tests cover `commit_source_file`'s
        // `guard()?`; this one covers *this* guard — the stat re-check — whose
        // refusal is what the watcher requeues on.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("a.md");
        std::fs::write(&path, "one").expect("write");
        let before = fs::metadata(&path).expect("stat");
        let guard = guard_against(&path, &before);
        std::thread::sleep(Duration::from_millis(20));
        assert!(guard().is_ok(), "an untouched file commits");

        let before = fs::metadata(&path).expect("stat");
        let guard = guard_against(&path, &before);
        // Different length rather than a different mtime: a same-tick rewrite
        // is the common case and the size is the half that cannot be faked.
        std::fs::write(&path, "a much longer body than before").expect("write");
        let refusal = guard().expect_err("a file that moved must refuse");
        assert!(
            matches!(refusal, StoreError::GuardRejected(_)),
            "the refusal keeps its own type: {refusal:?}"
        );
        assert!(
            refusal.to_string().contains("changed between"),
            "and says what happened: {refusal}"
        );
    }

    #[test]
    fn the_gate_verdict_answers_each_question_differently() {
        // The whole of D40 §4.2 / §4.3 in six assertions. Pinned as a pure
        // function because no portable test can change a file's bytes while
        // leaving its mtime and size alone — which is the one input that
        // distinguishes `Drop` from `ForceRebuild`.
        assert_eq!(
            verdict(false, false, None),
            Gate::Proceed,
            "no row at all, or a stat that differs, means there is work"
        );
        assert_eq!(verdict(false, true, None), Gate::Proceed);
        assert_eq!(
            verdict(true, false, None),
            Gate::Drop,
            "a settled stat that matches is a false event and is dropped"
        );
        assert_eq!(
            verdict(true, true, Some(true)),
            Gate::Proceed,
            "racy but the bytes agree: a row, not a rebuild"
        );
        assert_eq!(
            verdict(true, true, Some(false)),
            Gate::ForceRebuild,
            "the skip rule must not swallow a change the stat could not see"
        );
        assert_eq!(
            verdict(true, true, None),
            Gate::Proceed,
            "a torn read belongs to the retry, not to this decision"
        );
    }
}

pub(crate) fn now_epoch_secs() -> i64 {
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

/// The revision's metadata blob: the stat fingerprint, what the frontmatter
/// block turned out to be, and the two facts edge rebuilding replays from
/// (`§5 第 2/4/5 条`): the `links` extraction plus its `link_losses`, and the
/// `outline` anchors are validated against.
fn revision_metadata(
    fingerprint: &str,
    frontmatter: &FrontmatterParse,
    extracted: &Extracted,
    outline: &Outline,
) -> String {
    let mut blob = serde_json::from_str::<serde_json::Value>(fingerprint)
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = blob.as_object_mut() {
        object.insert("frontmatter".to_owned(), frontmatter_json(frontmatter));
        object.insert(
            "links".to_owned(),
            serde_json::json!(
                extracted
                    .links
                    .iter()
                    .map(|link| serde_json::json!({
                        "raw_target": link.raw_target,
                        "edge_kind": link.edge_kind.as_str(),
                        "link_source": link.link_source.as_str(),
                        "anchor_kind": link.anchor_kind.map(AnchorKind::as_str),
                        "anchor_raw": link.anchor_raw,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
        object.insert(
            "link_losses".to_owned(),
            serde_json::json!(
                extracted
                    .losses
                    .iter()
                    .map(|code| code.as_str())
                    .collect::<Vec<_>>()
            ),
        );
        object.insert(
            "outline".to_owned(),
            serde_json::json!({
                "headings": outline.headings,
                "block_ids": outline.block_ids,
            }),
        );
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

/// What a frontmatter block lost, in the enum's declaration order.
///
/// One home for the rule, because two paths need it: the indexing path and the
/// refreshed path, which used to re-derive the block and then throw the answer
/// away — a refreshed file with a broken block read as clean.
fn degradations_of(frontmatter: &FrontmatterParse) -> Vec<Degradation> {
    match frontmatter {
        // A JSON block that does not read is the same loss as a YAML block that
        // does not read, and the row says so.
        FrontmatterParse::Failed { .. } => vec![Degradation::FrontmatterParseError],
        FrontmatterParse::Json(raw) if json_frontmatter(raw).is_none() => {
            vec![Degradation::FrontmatterParseError]
        }
        FrontmatterParse::Absent | FrontmatterParse::Parsed(_) | FrontmatterParse::Json(_) => {
            Vec::new()
        }
    }
}

/// The code lists one coverage row carries (`ingest-interop.md` §5 第 8 条):
/// what the file lost, the warnings it earned (D42), and — for a
/// `windows_incompatible_name` row — the components the warning is about. One
/// bundle, so the row writer takes one payload argument instead of three, and
/// a third vocabulary would not grow its parameter list again.
struct RowPayload<'a> {
    degradations: &'a [Degradation],
    warnings: &'a [Warning],
    windows_names: &'a [String],
    /// Every ambiguous link of the file: the writing and the candidates it
    /// matched (`§5 第 3 条`) — the audit a no-edge decision leaves behind.
    ambiguous: &'a [(String, Vec<String>)],
    /// Every checked anchor that no longer exists in its target (`§5 第 4 条`).
    stale: &'a [(String, String)],
}

/// The `data_json` payload for one coverage row, or `{}` when it carries none.
///
/// Pure, so the shape has a test without a store: each list is written only
/// when it is non-empty, and a row that lost and earned nothing is exactly
/// `{}`.
fn payload_json(payload: &RowPayload<'_>) -> String {
    let mut object = serde_json::Map::new();
    if !payload.degradations.is_empty() {
        object.insert(
            "degradations".to_owned(),
            serde_json::json!(
                payload
                    .degradations
                    .iter()
                    .map(|code| code.as_str())
                    .collect::<Vec<_>>()
            ),
        );
    }
    if !payload.warnings.is_empty() {
        object.insert(
            "warnings".to_owned(),
            serde_json::json!(
                payload
                    .warnings
                    .iter()
                    .map(|code| code.as_str())
                    .collect::<Vec<_>>()
            ),
        );
    }
    if !payload.windows_names.is_empty() {
        object.insert(
            "windows_incompatible_names".to_owned(),
            serde_json::json!(payload.windows_names),
        );
    }
    if !payload.stale.is_empty() {
        object.insert(
            "stale_anchors".to_owned(),
            serde_json::json!(
                payload
                    .stale
                    .iter()
                    .map(|(target, anchor)| serde_json::json!({
                        "target": target,
                        "anchor": anchor,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    if !payload.ambiguous.is_empty() {
        object.insert(
            "ambiguous".to_owned(),
            serde_json::json!(
                payload
                    .ambiguous
                    .iter()
                    .map(|(raw, candidates)| serde_json::json!({
                        "raw_target": raw,
                        "candidates": candidates,
                    }))
                    .collect::<Vec<_>>()
            ),
        );
    }
    serde_json::Value::Object(object).to_string()
}

/// Appends one `ingest.coverage` row, and records the failure when there is one.
///
/// Three steps live here — bump `seq`, append the row, push the failure — and so
/// do both renderings: a refusal's `reason_code` is the typed kind's spelling and
/// its message is the very string the report carries. No call site re-types
/// either, the row cannot disagree with the report, and no clone is needed to
/// keep them equal.
// Ten arguments, the same count as before the warnings landed: the three code
// lists ride the one `RowPayload` bundle (one more explicit list would have
// made twelve). The wider context struct a review measured was +22 lines and
// fixed nothing; the rule this function exists for is one home per rendering,
// not fewer parameters.
#[allow(clippy::too_many_arguments)]
fn record_coverage(
    store: &mut Store,
    report: &mut IndexReport,
    batch: &mut Batch,
    source_key: Option<&str>,
    outcome: &str,
    reason_code: Option<&str>,
    message: Option<&str>,
    data_json: &str,
    failure: Option<(IndexFileErrorKind, String)>,
) -> Result<(), RunError> {
    let refusal = failure.as_ref().map(|(kind, _)| kind.to_string());
    let reason_code = refusal.as_deref().or(reason_code);
    let message = failure.as_ref().map(|(_, text)| text.as_str()).or(message);
    let seq = batch.next_seq();
    store.append_store_event(&coverage_event(
        batch,
        seq,
        source_key,
        outcome,
        reason_code,
        message,
        data_json,
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
            "the path is a symlink; it was recorded, not followed (D41.9)"
        }
        Degradation::AttachmentUnindexed => {
            "the file was registered; its bytes are not searchable on purpose (D41.10)"
        }
    }
}

/// What one warning code means, as the row's message.
///
/// Exhaustive on purpose, like [`degradation_message`]: a warning with no
/// sentence is a compile error. The sentences describe the fact, not the row
/// state — the same code rides an `indexed` row and a `skipped` one (§5 第 8 条 ⑤).
fn warning_message(code: Warning) -> &'static str {
    match code {
        Warning::OneDrivePlaceholder => {
            "the file is a cloud placeholder; its bytes are not on this device, so nothing was read (D42)"
        }
        Warning::IcloudPlaceholder => {
            "the file is an iCloud eviction stub; the note it stands for is not on this device (D42)"
        }
        Warning::WindowsIncompatibleName => {
            "the name is one Windows' naming rules reserve; tools and sync clients that follow them will refuse it (D42)"
        }
        Warning::SourceRemoved => {
            "the path no longer holds a file; its rows stay as history and the next reconcile converges the projections (D40 §4.2)"
        }
    }
}

/// One file's store step, with the call injected.
///
/// A store refusal here is engine state a test cannot portably provoke — the file
/// is a real file, the database real and healthy — so the shape is replayed
/// rather than provoked, on the `discard_with` precedent. The kind is written once,
/// here, so the store calls one pass makes cannot drift apart.
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
    index_workspace_with(root, path_override, attributes_of)
}

/// [`index_workspace`], with the file-attribute reader injected.
///
/// The one fact this process cannot obtain on most machines is the Windows
/// placeholder bit, and no test on any platform can fabricate a real one (only
/// kernel-mode callers can set it). This parameter is the seam that keeps the
/// branch honest anyway, on the `doctor_with_facts` precedent: production
/// passes `momotaro_ingest::attributes_of` (the real attribute word on
/// Windows, a constant zero elsewhere), and the tests inject facts to drive
/// what no machine here can build.
pub fn index_workspace_with(
    root: impl AsRef<Path>,
    path_override: Option<&Path>,
    attributes_of: impl Fn(&Path) -> io::Result<u32>,
) -> Result<IndexReport, RunError> {
    Ok(run_batch(
        root,
        path_override,
        attributes_of,
        BatchShape::full(INGEST_PLAN),
    )?
    .report)
}

/// The batch loop, with the event family it writes injected (D40 §4.4).
///
/// `index` hands it a full [`BatchShape`] over [`INGEST_PLAN`]; the four
/// reconciliation triggers hand it a full one over [`RECONCILE_PLAN`]; the
/// watcher hands it a gated subset. The walk, the comparison, the skip rule and
/// the projection are deliberately *not* parameterised: they are one piece of
/// work, and a second copy of them is how a `touch` ends up reported as a new
/// revision while the store holds a single row.
pub(crate) fn run_batch(
    root: impl AsRef<Path>,
    path_override: Option<&Path>,
    attributes_of: impl Fn(&Path) -> io::Result<u32>,
    shape: BatchShape,
) -> Result<BatchOutcome, RunError> {
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

    let entries = walk_entries(&vault_root)?;

    // The namespace every link resolves against (§5 第 3 条): every walked
    // path that has a key, keyed by its vault-relative tail. Built before any
    // processing so resolution never depends on walk order — a link to a file
    // processed later resolves exactly like one processed earlier.
    let mut namespace_pairs: Vec<(String, String)> = Vec::new();
    // Attachment and canvas keys ride the same pass, for the anchor carve-out
    // (§5 第 4 条): a fragment on such a target is not an anchor — a PDF page
    // number, a canvas subpath, neither is a heading.
    let mut non_note_keys: HashSet<String> = HashSet::new();
    for entry in &entries {
        let Ok(key) = source_key_for(&vault_root, &entry.path) else {
            continue;
        };
        let tail = key.strip_prefix("note:").unwrap_or(&key).to_owned();
        namespace_pairs.push((tail, key.clone()));
        if matches!(
            entry.kind,
            VaultFileKind::Attachment | VaultFileKind::Canvas
        ) {
            non_note_keys.insert(key);
        }
    }

    let mut batch = Batch::new(shape.plan);

    let mut report = IndexReport {
        batch_id: batch.id.clone(),
        files_scanned: entries.len() as u64,
        files_skipped: 0,
        files_indexed: 0,
        revisions_created: 0,
        chunks_written: 0,
        errors: Vec::new(),
        duration_ms: 0,
    };

    // D40 §4.2: a watcher's path is a hint, not a fact. The cheap pass runs
    // *before* the batch opens, so a false event costs one stat and writes
    // nothing at all — not even `started`.
    let mut forced: HashMap<PathBuf, bool> = HashMap::new();
    if shape.gated {
        // Both answers are about the volume and the batch, not about the file:
        // read them once for the whole loop (D40 §4.3).
        let coarse = momotaro_platform::volume_info(&vault_root).coarse == Some(true);
        let last_written = last_written(&store);
        for path in shape.only.iter().flatten() {
            match gate(
                &store,
                &workspace_root,
                &vault_root,
                path,
                coarse,
                last_written,
            )? {
                Gate::Drop | Gate::Absent => {}
                Gate::ForceRebuild => {
                    forced.insert(path.clone(), true);
                }
                Gate::Proceed => {
                    forced.insert(path.clone(), false);
                }
                // Everything the walk found under this directory: the events
                // named the directory alone, so its files have had none.
                Gate::Directory => {
                    for entry in &entries {
                        if entry.path.starts_with(path) {
                            forced.entry(entry.path.clone()).or_insert(false);
                        }
                    }
                }
            }
        }
        report.files_scanned = forced.len() as u64;
    }

    // Deletions, at vault scale (D40 §4.2's delete half): a key the walk no
    // longer names. The row cannot repeat, because the demotion that goes with
    // it takes the key out of `current_sources` — and a file that comes back
    // becomes current again, so its *next* disappearance is a new fact.
    let keep_keys: HashSet<&str> = namespace_pairs
        .iter()
        .map(|(_, key)| key.as_str())
        .collect();
    // A walk that did not cover the whole vault cannot say what is absent:
    // `index --vault sub` names fewer files on purpose, and every source
    // outside the override would look deleted. Absence is a whole-vault
    // question, so a narrowed walk leaves it alone.
    let mut removals: Vec<PendingRow> = Vec::new();
    if path_override.is_none() {
        for (key, raw_name, _ingested_at) in store.current_sources()? {
            if keep_keys.contains(key.as_str()) {
                continue;
            }
            // The row is attributed by name, and a row written before
            // `raw_name` existed has none: fall back to the key's own spelling,
            // which is the vault-relative path of the file that is no longer
            // there.
            let raw_name = raw_name.or_else(|| key.strip_prefix("note:").map(str::to_owned));
            removals.push(PendingRow {
                source_key: Some(key),
                raw_name,
                outcome: Ok(FileOutcome::Skipped {
                    degradations: Vec::new(),
                    warnings: vec![Warning::SourceRemoved],
                }),
                row_written: false,
                replace_rows: None,
                anchors: Vec::new(),
                ambiguous: Vec::new(),
            });
        }
    }

    if shape.gated && forced.is_empty() && removals.is_empty() {
        report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return Ok(BatchOutcome {
            report,
            removed: removals.len() as u64,
            retry: Vec::new(),
        });
    }

    // Opened here, not above: a batch the gate threw away must not pay for an
    // index reader and a 50 MB writer it will never hand anything to.
    fs::create_dir_all(index_dir(root)).map_err(|source| RunError::Io { source })?;
    let search_index = SearchIndex::open_or_create(&index_dir(root))?;
    let mut writer = search_index.writer(WRITER_HEAP_BYTES)?;

    // The batch's own edges (ADR 0026): `started` before any file work, so a
    // reader can tell "running" from "never started"; `finished` last, after
    // every per-file row, so a batch is never called complete while a file is
    // still unrecorded.
    let seq = batch.next_seq();
    store.append_store_event(&NewStoreEvent {
        occurred_at: now_epoch_secs(),
        batch_id: &batch.id,
        seq,
        domain: batch.plan.domain,
        event_type: batch.plan.started,
        source_key: None,
        outcome: None,
        reason_code: None,
        message: None,
        data_json: &serde_json::json!({ "files_scanned": report.files_scanned }).to_string(),
        external_id: None,
    })?;

    let removed = removals.len() as u64;
    let ns = Namespace::new(
        namespace_pairs
            .iter()
            .map(|(tail, key)| (tail.as_str(), key.as_str())),
    );

    let mut claimed: HashMap<String, String> = HashMap::new();
    // Coverage rows are written after the data batch (ADR 0026: data first,
    // events after): `stale_anchor` is decided against the state every file
    // left behind, never against walk order (§5 第 4 条). A watcher's rows are
    // already in the store by the time they get here (D40 §4.7).
    // The store half and the derived half of the same fact: a revision that
    // is no longer current stops a `rebuild-index` from resurrecting it, drops
    // it out of the link namespace, and takes it out of the next walk's
    // absence check — while the document goes out of the search index, which
    // is the part a user can see.
    for row in &removals {
        if let Some(key) = &row.source_key {
            store.demote_current(key)?;
            SearchIndex::remove_source(&mut writer, search_index.fields(), key)?;
        }
    }
    let mut pending: Vec<PendingRow> = removals;
    let mut retry: Vec<PathBuf> = Vec::new();

    let ctx = PassContext {
        vault_root: &vault_root,
        workspace_root: &workspace_root,
        policy,
        ns: &ns,
        non_note_keys: &non_note_keys,
        attributes_of: &attributes_of,
        search_index: &search_index,
    };

    for entry in entries {
        let file = &entry.path;
        // A path the gate already threw away has nothing to say; anything
        // left is a file this batch actually intends to write. `gated` and a
        // path subset are the same question — `BatchShape::watched` is the only
        // way to have one, and it sets the other — so this is the single place
        // that decides which paths a batch touches.
        if shape.gated && !forced.contains_key(file) {
            continue;
        }
        let mode = if shape.gated {
            ProcessMode::Watch {
                force_content: forced.get(file).copied().unwrap_or(false),
                batch: &mut batch,
            }
        } else {
            ProcessMode::Batch
        };
        let passed = pass_one(
            entry.kind,
            file,
            &ctx,
            &mut claimed,
            &mut store,
            &mut writer,
            &mut report,
            mode,
        )?;
        match passed {
            PassOne::Failed {
                source_key,
                raw_name,
                error,
                row_written,
            } => pending.push(PendingRow {
                source_key,
                raw_name,
                outcome: Err(error),
                row_written,
                replace_rows: None,
                anchors: Vec::new(),
                ambiguous: Vec::new(),
            }),
            PassOne::Torn {
                source_key,
                raw_name,
            } => {
                retry.push(file.to_path_buf());
                // The batch records what it could not read; the watcher backs
                // off and tries again, because a row here would claim a file
                // was read when it never was (D40 §4.5).
                if !shape.gated {
                    pending.push(PendingRow {
                        source_key: Some(source_key.clone()),
                        raw_name: Some(raw_name.clone()),
                        outcome: Err(file_error(
                            Some(&source_key),
                            IndexFileErrorKind::Io,
                            TORN_MESSAGE,
                        )),
                        row_written: false,
                        replace_rows: None,
                        anchors: Vec::new(),
                        ambiguous: Vec::new(),
                    });
                }
            }
            // A guard refusal wrote nothing, so a gated batch has no row to
            // write — only the retry (D40 §4.7). A batch has no retry queue,
            // so it says what happened instead of dropping the file silently.
            PassOne::Requeued {
                source_key,
                raw_name,
            } => {
                retry.push(file.to_path_buf());
                if !shape.gated {
                    pending.push(PendingRow {
                        source_key: Some(source_key.clone()),
                        raw_name: Some(raw_name.clone()),
                        outcome: Err(file_error(
                            Some(&source_key),
                            IndexFileErrorKind::Store,
                            GUARD_REFUSED,
                        )),
                        row_written: false,
                        replace_rows: None,
                        anchors: Vec::new(),
                        ambiguous: Vec::new(),
                    });
                }
            }
            PassOne::Done {
                source_key,
                raw_name,
                outcome,
                findings,
                row_written,
            } => pending.push(PendingRow {
                source_key: Some(source_key),
                raw_name: Some(raw_name),
                outcome,
                // Under the watcher `pass_one` has already written the row —
                // and, with it, the edges this projection owes.
                row_written,
                replace_rows: if shape.gated {
                    None
                } else {
                    findings.replace_rows
                },
                anchors: findings.anchors,
                ambiguous: findings.ambiguous,
            }),
        }
    }

    writer.commit()?;

    // The data sweep: every projection-only row set commits together — one
    // transaction for the batch, each source still wholesale (ADR 0027) — and
    // the rows of sources this walk no longer names go with it: a rebuilt table
    // never produced them, so leaving them would break "a rebuild reproduces
    // every row".
    let mut projections: Vec<(String, Vec<GraphLink>)> = Vec::new();
    for row in &mut pending {
        if let (Some(key), Some(rows)) = (row.source_key.clone(), row.replace_rows.take()) {
            projections.push((key, rows));
        }
    }
    if !projections.is_empty() {
        store.replace_edges_many(&projections)?;
    }
    let keep: Vec<&str> = namespace_pairs
        .iter()
        .map(|(_, key)| key.as_str())
        .collect();
    let keep_json = serde_json::to_string(&keep).expect("keys serialize");
    store.prune_graph_edges_outside(&keep_json)?;
    let (graph_rows, graph_dangling) = store.graph_counts()?;

    // The event sweep: every file's data — chunks, blobs, rows — is settled, so
    // an anchor check sees the run's final state and the codes cannot depend on
    // walk order (§5 第 4 条). Data first, events after (ADR 0026).
    let mut graph_ambiguous = 0u64;
    let mut graph_stale = 0u64;
    for row in pending {
        let stale = stale_anchors(&store, &row.anchors, &non_note_keys)?;
        graph_ambiguous += row.ambiguous.len() as u64;
        graph_stale += stale.len() as u64;
        // A watcher's row rode its file's transaction: counting it here is the
        // sweep's whole remaining job (ADR 0026 never meant two rows).
        if !row.row_written {
            flush_row(&mut store, &mut report, &mut batch, row, stale)?;
        }
    }

    let seq = batch.next_seq();
    store.append_store_event(&NewStoreEvent {
        occurred_at: now_epoch_secs(),
        batch_id: &batch.id,
        seq,
        domain: batch.plan.domain,
        event_type: batch.plan.finished,
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
            "graph_rows": graph_rows,
            "graph_dangling": graph_dangling,
            "graph_ambiguous_links": graph_ambiguous,
            "graph_stale_anchors": graph_stale,
        })
        .to_string(),
        external_id: None,
    })?;

    report.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(BatchOutcome {
        report,
        removed,
        retry,
    })
}

/// One `ingest.coverage` row for one file (ADR 0026).
///
/// `outcome` is one of `indexed | skipped | metadata_refreshed | error`.
///
/// `reason_code` is set in four cases, and a reader must branch on the pair,
/// never on the outcome alone: an `error` row carries the stable spelling of the
/// error kind; an `indexed` or `metadata_refreshed` row carries the degradation
/// code of something the file lost on the way in (`frontmatter_parse_error`,
/// D41.5) — it is in the index anyway; a `skipped` row carries the code of what
/// kept it out (`symlink_encountered`, §5 第 10 条); and any non-`error` row
/// whose file earned a D42 warning carries it when no degradation outranks it
/// (`onedrive_placeholder` and `icloud_placeholder` on `skipped` rows,
/// `windows_incompatible_name` on every read-state — §5 第 8 条 ⑤).
fn coverage_event<'a>(
    batch: &'a Batch,
    seq: i64,
    source_key: Option<&'a str>,
    outcome: &'a str,
    reason_code: Option<&'a str>,
    message: Option<&'a str>,
    data_json: &'a str,
) -> NewStoreEvent<'a> {
    NewStoreEvent {
        occurred_at: now_epoch_secs(),
        batch_id: &batch.id,
        seq,
        domain: batch.plan.domain,
        event_type: batch.plan.coverage,
        source_key,
        outcome: Some(outcome),
        reason_code,
        message,
        data_json,
        external_id: None,
    }
}

/// What one file's pass through [`process_file`] did (ADR 0026's outcomes).
///
/// `Clone` because a watcher row is built from the outcome *and* returned to
/// its caller, and building the same outcome twice would let the two disagree.
#[derive(Debug, Clone)]
pub(crate) enum FileOutcome {
    /// Nothing was read: the fingerprint, stored path and raw name all match, or
    /// the file is a link the index does not follow, an eviction stub it does not
    /// read, or a placeholder whose bytes are not on this device. `degradations`
    /// and `warnings` carry what that decision cost the file, so a link's row
    /// says why it is not searchable.
    Skipped {
        degradations: Vec<Degradation>,
        warnings: Vec<Warning>,
    },
    /// Same bytes under a new path or spelling: the mutable columns moved. The
    /// list is re-derived on this path too, or a file whose block cannot be read
    /// would read as clean here (§5 第 8 条).
    MetadataRefreshed {
        degradations: Vec<Degradation>,
        warnings: Vec<Warning>,
    },
    /// A revision and its chunks were written. `degradations` carries whatever the
    /// file lost on the way in, in the enum's declaration order — the file *was*
    /// indexed, so the outcome stays `indexed` and the codes ride alongside it. A
    /// file can lose several things at once (§5 第 8 条), so it is a list: the row's
    /// `reason_code` is its first element and its `data_json` carries all of it.
    Indexed {
        degradations: Vec<Degradation>,
        warnings: Vec<Warning>,
    },
}

impl FileOutcome {
    /// The wire spelling of the `indexed` outcome. A reader that only needs the
    /// word compares against this; the variant itself carries the degradation
    /// code beside the outcome.
    const INDEXED: &'static str = "indexed";

    /// The wire spelling of the `metadata_refreshed` outcome, for the same reason
    /// the indexed one exists: a struct variant is not a value to compare against.
    const METADATA_REFRESHED: &'static str = "metadata_refreshed";

    /// The wire spelling of `skipped`, for the same reason again.
    const SKIPPED: &'static str = "skipped";

    /// The wire spelling written to `store_events.outcome` (ADR 0026).
    fn as_str(&self) -> &'static str {
        match self {
            Self::Skipped { .. } => Self::SKIPPED,
            Self::MetadataRefreshed { .. } => "metadata_refreshed",
            Self::Indexed { .. } => Self::INDEXED,
        }
    }

    /// The whole degradation list: the row's `data_json` and the per-code
    /// counts read it (§5 第 8 条 ③④).
    fn degradations(&self) -> &[Degradation] {
        match self {
            Self::Skipped { degradations, .. }
            | Self::MetadataRefreshed { degradations, .. }
            | Self::Indexed { degradations, .. } => degradations,
        }
    }

    /// The whole warning list (D42), in the enum's declaration order: the row's
    /// `data_json` and the per-code counts read it, the same way they read the
    /// degradations.
    fn warnings(&self) -> &[Warning] {
        match self {
            Self::Skipped { warnings, .. }
            | Self::MetadataRefreshed { warnings, .. }
            | Self::Indexed { warnings, .. } => warnings,
        }
    }

    /// The first degradation, for callers that need its sentence rather than its
    /// spelling — the row's primary code when the file lost something.
    fn degradation(&self) -> Option<Degradation> {
        self.degradations().first().copied()
    }

    /// Adds one code and restores the declaration order (§5 第 8 条).
    ///
    /// The event sweep calls this: `stale_anchor` is decided against the run's
    /// final state, after every file's data is written (§5 第 4 条), so it joins
    /// the list last while the row still reads in enum order.
    fn push_degradation(&mut self, code: Degradation) {
        let list = match self {
            Self::Skipped { degradations, .. }
            | Self::MetadataRefreshed { degradations, .. }
            | Self::Indexed { degradations, .. } => degradations,
        };
        list.push(code);
        list.sort_unstable();
        list.dedup();
    }
}

/// What the caller wants from one file's pass (D40 §4.2 / §4.3 / §4.7).
pub(crate) enum ProcessMode<'a> {
    /// The batch. The walk took the stat this pass compares against, the
    /// coverage rows ride the batch-tail sweep (ADR 0026), and a matching
    /// fingerprint is enough to skip.
    Batch,
    /// The watcher. A matching fingerprint is *not* enough when the stat is
    /// racy, the file's own event rides the file's own transaction, and that
    /// transaction re-checks the file's stat as its first step.
    Watch {
        /// Bypass the skip branch: the stat matched but the bytes did not.
        force_content: bool,
        /// The batch this file's row takes its sequence from.
        batch: &'a mut Batch,
    },
}

/// The things every file's pass reads and nothing inside it changes.
///
/// Bundled rather than passed one at a time: there are eight of them, the
/// batch and the watcher must hand over the *same* eight, and a signature
/// that long is how one of them quietly gets omitted at a call site.
pub(crate) struct PassContext<'a> {
    /// The vault the key is derived against.
    pub vault_root: &'a Path,
    /// The workspace every stored path is relative to (ADR 0024).
    pub workspace_root: &'a Path,
    /// The chunking policy.
    pub policy: &'a IndexPolicy,
    /// The namespace this batch's links resolve against (§5 第 3 条).
    pub ns: &'a Namespace,
    /// Attachment and canvas keys, for the anchor carve-out (§5 第 4 条).
    pub non_note_keys: &'a HashSet<String>,
    /// The injected attribute reader (see [`index_workspace_with`]).
    pub attributes_of: &'a dyn Fn(&Path) -> io::Result<u32>,
    /// The open search index.
    pub search_index: &'a SearchIndex,
}

/// What one file's pass produced, before anything is written for it.
///
/// The batch turns every variant into a row at its tail; the watcher writes
/// the row inside this call, because a watcher row has to land with the data
/// it describes (D40 §4.7). Both callers see the same outcomes — the half
/// where the two orchestrations must not disagree is this function, not
/// either of them.
pub(crate) enum PassOne {
    /// Nothing about this path could be derived, or it was refused. Whatever
    /// of the key and the name could be had comes along, because the row is
    /// attributed with them.
    Failed {
        /// The key this row is about, when one could be derived.
        source_key: Option<String>,
        /// The name, when the key was derived but the name was not.
        raw_name: Option<String>,
        /// The classified failure.
        error: IndexFileError,
        /// Whether this call already wrote the refusal's row: `true` under
        /// [`ProcessMode::Watch`], `false` under [`ProcessMode::Batch`].
        row_written: bool,
    },
    /// The file changed while it was read, so the buffer was never one
    /// version of it. Nothing was written and there is no row — the batch
    /// records an error row from here, the watcher backs off and requeues
    /// (D40 §4.5).
    Torn {
        /// The key this path would have had.
        source_key: String,
        /// The name it was spelled with.
        raw_name: String,
    },
    /// The guard refused: the unit rolled back and nothing was written, so
    /// there is nothing to record — the caller requeues (D40 §4.7).
    Requeued {
        /// The key this path would have had.
        source_key: String,
        /// The name it was spelled with.
        raw_name: String,
    },
    /// The file was passed.
    Done {
        /// The key the row is about.
        source_key: String,
        /// The name it was spelled with.
        raw_name: String,
        /// What the file's pass concluded.
        outcome: Result<FileOutcome, IndexFileError>,
        /// The part of the projection the caller still owes work for.
        findings: LinkFindings,
        /// Whether this call already wrote the row: `true` under
        /// [`ProcessMode::Watch`], always `false` under [`ProcessMode::Batch`],
        /// whose rows ride the batch tail.
        row_written: bool,
    },
}

/// The sentence a torn read gets on a row (D40 §4.5).
const TORN_MESSAGE: &str = "the file changed while it was being read; it was not indexed";

/// What a guard refusal says, wherever it surfaces (D40 §4.7).
///
/// One home: the guard itself, and the row a *batch* writes, because a batch
/// has no retry queue to hand the refusal to. The watcher writes no row at
/// all — the unit rolled back, so there is nothing to report.
const GUARD_REFUSED: &str = "the file changed between the read and the commit";

/// Writes one refusal's row now, when the caller asked for watcher rows, and
/// does nothing when the caller will write it at the batch tail.
///
/// The batch keeps its rows for the tail on purpose (ADR 0026: every file's
/// data settles first, so `stale_anchor` cannot depend on walk order), so this
/// is the one place the two timings are allowed to differ.
fn row_now(
    store: &mut Store,
    report: &mut IndexReport,
    mode: &mut ProcessMode<'_>,
    row: PendingRow,
    stale: Vec<(String, String)>,
) -> Result<(), RunError> {
    match mode {
        ProcessMode::Batch => Ok(()),
        ProcessMode::Watch { batch, .. } => flush_row(store, report, batch, row, stale),
    }
}

/// The `PendingRow` a refusal becomes, so both callers build the same shape.
fn refusal_row(
    source_key: Option<&str>,
    raw_name: Option<&str>,
    error: IndexFileError,
) -> PendingRow {
    PendingRow {
        source_key: source_key.map(str::to_owned),
        raw_name: raw_name.map(str::to_owned),
        outcome: Err(error),
        row_written: false,
        replace_rows: None,
        anchors: Vec::new(),
        ambiguous: Vec::new(),
    }
}

/// Runs one file's whole pass: derive the key and the name, refuse a second
/// claimant, run the placeholder gate, dispatch on the kind.
///
/// Shared by the batch loop and by the watcher so the two cannot disagree
/// about what a path is, whether its bytes may be read, or what a collision
/// costs (D42). Under [`ProcessMode::Watch`] the row is written before this
/// returns; under [`ProcessMode::Batch`] it is not — see [`PassOne::Done`].
/// Anything that fails at batch level (rather than at *this file*) comes back
/// as [`RunError`], because a row is not the way to report a dead store.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pass_one(
    kind: VaultFileKind,
    file: &Path,
    ctx: &PassContext<'_>,
    claimed: &mut HashMap<String, String>,
    store: &mut Store,
    writer: &mut IndexWriterHandle,
    report: &mut IndexReport,
    mut mode: ProcessMode<'_>,
) -> Result<PassOne, RunError> {
    let watch = matches!(mode, ProcessMode::Watch { .. });

    let source_key = match source_key_for(ctx.vault_root, file) {
        Ok(key) => key,
        // Reachable codes here: `invalid_utf8`, `outside_vault`,
        // `invalid_source_key` (see `IngestError::kind`). A non-UTF-8 name
        // cannot be created on Windows, so no portable test drives this arm;
        // it stays correct by construction because the row's code is the
        // typed kind's spelling, never a literal.
        Err(error) => {
            let message = error.to_string();
            let error = file_error(None, error.kind(), message);
            let row = refusal_row(None, None, error.clone());
            row_now(store, report, &mut mode, row, Vec::new())?;
            return Ok(PassOne::Failed {
                row_written: watch,
                source_key: None,
                raw_name: None,
                error,
            });
        }
    };

    let raw_name = match raw_name_for(ctx.vault_root, file) {
        Ok(name) => name,
        Err(error) => {
            let message = error.to_string();
            let error = file_error(Some(&source_key), error.kind(), message);
            let row = refusal_row(Some(&source_key), None, error.clone());
            row_now(store, report, &mut mode, row, Vec::new())?;
            return Ok(PassOne::Failed {
                row_written: watch,
                source_key: Some(source_key),
                raw_name: None,
                error,
            });
        }
    };

    // D42: two distinct files must never fold into one identity — NFC and NFD
    // spellings of one name, or two paths to one object. The store event table
    // exists (schema v4, ADR 0026) and this refusal is one of its rows
    // (`outcome = error`, `reason_code = key_collision`); what is still missing
    // is the disambiguating-suffix policy, so the pass refuses the second
    // claimant instead of silently overwriting the first.
    if let Some(first) = claimed.get(&source_key) {
        let message = format!("claimed by both {first:?} and {raw_name:?}");
        let error = file_error(Some(&source_key), IndexFileErrorKind::KeyCollision, message);
        let row = refusal_row(Some(&source_key), Some(&raw_name), error.clone());
        row_now(store, report, &mut mode, row, Vec::new())?;
        return Ok(PassOne::Failed {
            row_written: watch,
            source_key: Some(source_key),
            raw_name: Some(raw_name),
            error,
        });
    }
    claimed.insert(source_key.clone(), raw_name.clone());

    // D42: a cloud placeholder's bytes are not on this device, and nothing may
    // read them. The attributes come through the injected reader (production:
    // the real word on Windows, a constant zero elsewhere), and the question
    // runs before any content read of any kind that has content.
    if matches!(
        kind,
        VaultFileKind::Attachment | VaultFileKind::Note | VaultFileKind::Canvas
    ) {
        match (ctx.attributes_of)(file) {
            Ok(attrs) if is_cloud_placeholder(attrs) => {
                report.files_skipped += 1;
                let outcome = FileOutcome::Skipped {
                    degradations: Vec::new(),
                    warnings: vec![Warning::OneDrivePlaceholder],
                };
                let row = PendingRow {
                    source_key: Some(source_key.clone()),
                    raw_name: Some(raw_name.clone()),
                    outcome: Ok(outcome.clone()),
                    row_written: false,
                    replace_rows: None,
                    anchors: Vec::new(),
                    ambiguous: Vec::new(),
                };
                row_now(store, report, &mut mode, row, Vec::new())?;
                return Ok(PassOne::Done {
                    source_key,
                    raw_name,
                    outcome: Ok(outcome),
                    findings: LinkFindings::default(),
                    row_written: watch,
                });
            }
            Ok(_) => {}
            Err(error) => {
                let message = error.to_string();
                let error = file_error(Some(&source_key), IndexFileErrorKind::Io, message);
                let row = refusal_row(Some(&source_key), Some(&raw_name), error.clone());
                row_now(store, report, &mut mode, row, Vec::new())?;
                return Ok(PassOne::Failed {
                    row_written: watch,
                    source_key: Some(source_key),
                    raw_name: Some(raw_name),
                    error,
                });
            }
        }
    }

    let (outcome, findings) = match kind {
        // A link is not content: this version does not follow them (the vault
        // layer says so by kind), so the row is the whole record — no revision,
        // no chunks, and nothing is ever read through the link (§5 第 10 条).
        VaultFileKind::Symlink => {
            report.files_skipped += 1;
            (
                Ok(FileOutcome::Skipped {
                    degradations: vec![Degradation::SymlinkEncountered],
                    warnings: Vec::new(),
                }),
                LinkFindings::default(),
            )
        }
        // An iCloud eviction stub is not content either: the note it stands for
        // is not on this device, so the row is the whole record and the warning
        // says why (D42).
        VaultFileKind::IcloudPlaceholder => {
            report.files_skipped += 1;
            (
                Ok(FileOutcome::Skipped {
                    degradations: Vec::new(),
                    warnings: vec![Warning::IcloudPlaceholder],
                }),
                LinkFindings::default(),
            )
        }
        // Everything that is not a note is registered rather than read: the
        // vault's inventory is complete without pretending binary bytes are
        // searchable (§5 第 10 条).
        VaultFileKind::Attachment => (
            register_attachment(
                store,
                ctx.workspace_root,
                file,
                &source_key,
                &raw_name,
                report,
            ),
            LinkFindings::default(),
        ),
        // The note path owns its own transaction, so it returns here: a watcher
        // row is written *inside* `process_file`, beside the data it describes
        // (D40 §4.7), while a batch row is written at the tail.
        VaultFileKind::Note | VaultFileKind::Canvas => {
            let passed = process_file(
                kind,
                store,
                ctx.search_index,
                writer,
                ctx.workspace_root,
                file,
                &source_key,
                &raw_name,
                ctx.policy,
                report,
                ctx.ns,
                ctx.non_note_keys,
                &mut mode,
            );
            return match passed {
                Ok((outcome, findings)) => Ok(PassOne::Done {
                    source_key,
                    raw_name,
                    outcome: Ok(outcome),
                    findings,
                    row_written: watch,
                }),
                Err(PassError::Torn) => Ok(PassOne::Torn {
                    source_key,
                    raw_name,
                }),
                Err(PassError::Guard) => Ok(PassOne::Requeued {
                    source_key,
                    raw_name,
                }),
                Err(PassError::File(error)) => {
                    // `process_file` owns this row: it already wrote it when
                    // the caller asked for watcher rows, and writing it here
                    // too gave one failing file two rows in one batch.
                    Ok(PassOne::Failed {
                        row_written: watch,
                        source_key: Some(source_key),
                        raw_name: Some(raw_name),
                        error,
                    })
                }
                Err(PassError::Run(error)) => Err(error),
            };
        }
    };

    // Only arms that never touched `process_file` reach here, so `mode` is
    // still the caller's and the row's timing is still its choice.
    if watch {
        let stale = stale_anchors(store, &findings.anchors, ctx.non_note_keys)?;
        let row = PendingRow {
            source_key: Some(source_key.clone()),
            raw_name: Some(raw_name.clone()),
            outcome: outcome.clone(),
            row_written: false,
            replace_rows: findings.replace_rows.clone(),
            anchors: findings.anchors.clone(),
            ambiguous: findings.ambiguous.clone(),
        };
        row_now(store, report, &mut mode, row, stale)?;
        if let Some(rows) = findings.replace_rows.clone() {
            store.replace_edges_many(&[(source_key.clone(), rows)])?;
        }
    }

    Ok(PassOne::Done {
        source_key,
        raw_name,
        outcome,
        findings,
        row_written: watch,
    })
}

#[allow(clippy::too_many_arguments)]
fn process_body(
    kind: VaultFileKind,
    store: &mut Store,
    search_index: &SearchIndex,
    writer: &mut IndexWriterHandle,
    workspace_root: &Path,
    file: &Path,
    source_key: &str,
    raw_name: &str,
    policy: &IndexPolicy,
    report: &mut IndexReport,
    ns: &Namespace,
    non_note_keys: &HashSet<String>,
    mode: &mut ProcessMode<'_>,
    row_written: &mut bool,
) -> Result<(FileOutcome, LinkFindings), PassError> {
    let metadata = fs::metadata(file)
        .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?;
    let fingerprint = fingerprint_json(&metadata);
    // Workspace-relative and `/`-joined (ADR 0024). Computed before the skip
    // check on purpose: a directory move preserves the fingerprint, so a stored
    // path may only be assumed correct if it is compared.
    let local_path = momotaro_ingest::local_path_in(workspace_root, file)
        .map_err(|error| file_error(Some(source_key), error.kind(), error))?;
    let from_dir = from_dir_for(source_key);

    let current = store_step(source_key, || store.current_revision(source_key))?;

    if let Some(revision) = &current
        && fingerprint_matches(&revision.metadata_json, &metadata)
        && revision.local_path.as_deref() == Some(local_path.as_str())
        && revision.raw_name.as_deref() == Some(raw_name)
        // A racy stat that matched is not evidence of anything: D40 §4.3
        // sends it to the content path, where the hash decides.
        && !forced(mode)
    {
        report.files_skipped += 1;
        // Nothing was *read*, but the file's links still resolve against the
        // namespace as it stands today (ADR 0027): the rows are re-projected
        // from the stored extraction, and what they cost the file — a target
        // that vanished, a name that turned ambiguous, a decode loss the blob
        // remembers — rides the row (`§5 第 2 条` / `§5 第 7 条`). A blob from
        // before the keys existed reads as no links (迁移注记).
        let extracted = parse_link_blob(&revision.metadata_json);
        let projection = project_links(
            &extracted,
            source_key,
            from_dir,
            revision.ingested_at,
            ns,
            non_note_keys,
        );
        return Ok((
            FileOutcome::Skipped {
                degradations: projection.codes,
                warnings: Vec::new(),
            },
            LinkFindings {
                replace_rows: Some(projection.rows),
                anchors: projection.anchors,
                ambiguous: projection.ambiguous,
            },
        ));
    }

    let bytes = match read_stable(file)
        .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?
    {
        StableRead::Stable(bytes) => bytes,
        // The two stats disagreed, so the buffer is two versions of the file.
        // It is dropped here: D40 §4.5's absolute half is that torn bytes
        // never reach a caller that would commit them.
        StableRead::Torn => return Err(PassError::Torn),
    };
    let normalized = normalize_markdown(&bytes)
        .map_err(|error| file_error(Some(source_key), error.kind(), error))?;
    let hash = revision_hash(&normalized);

    if let Some(revision) = &current
        && revision.revision_hash == hash
    {
        // Same bytes, moved or re-spelled: refresh the mutable columns in place
        // instead of appending a revision that would say nothing new. What the file
        // lost is re-derived from the very text that hashed equal — one derivation,
        // and a workspace indexed before the `frontmatter` key existed gets it here.
        let input = chunk_input(&normalized, kind);
        let projection = project_links(
            &input.extracted,
            source_key,
            from_dir,
            revision.ingested_at,
            ns,
            non_note_keys,
        );
        // The codes were derived above for the blob; the same answer rides on
        // the row, or this path reports a broken block as a clean file.
        let mut outcome = FileOutcome::MetadataRefreshed {
            degradations: merge_codes(input.degradations, projection.codes),
            warnings: Vec::new(),
        };
        // One transaction for the refresh as well (D40 §4.7): the mutable
        // columns, the re-projected edges, and — under the watcher — this
        // file's row. The edges come along rather than waiting for the batch
        // sweep, so a refresh never leaves one half of the file behind.
        // `stale_anchor` is decided against the store as this file found it,
        // which for a one-file change *is* the settled state (§5 第 4 条). A
        // batch leaves it to its tail sweep, whose outcome it has not built yet.
        let stale = match mode {
            ProcessMode::Batch => Vec::new(),
            ProcessMode::Watch { .. } => stale_anchors(store, &projection.anchors, non_note_keys)?,
        };
        if !stale.is_empty() {
            outcome.push_degradation(Degradation::StaleAnchor);
        }
        // The payload is only needed where the row rides this transaction: a
        // batch writes its row at the tail and would build the same string a
        // second time, so that arm assigns a placeholder nothing reads.
        let data_json;
        #[allow(unused_assignments)]
        let event = match mode {
            ProcessMode::Batch => {
                data_json = String::new();
                None
            }
            ProcessMode::Watch { batch, .. } => {
                let fields = row_fields(&outcome, raw_name, &projection.ambiguous, &stale);
                data_json = fields.data_json;
                let seq = batch.next_seq();
                Some(coverage_event(
                    batch,
                    seq,
                    Some(source_key),
                    fields.outcome,
                    fields.reason_code,
                    fields.message,
                    &data_json,
                ))
            }
        };
        commit_step(source_key, || {
            store.commit_source_refresh(
                source_key,
                &revision.revision_hash,
                &revision_metadata(
                    &fingerprint,
                    &input.frontmatter,
                    &input.extracted,
                    &input.outline,
                ),
                &local_path,
                raw_name,
                &projection.rows,
                event.as_ref(),
                guard_against(file, &metadata),
            )
        })?;
        if event.is_some() {
            *row_written = true;
        }
        report.files_indexed += 1;
        return Ok((
            outcome,
            LinkFindings {
                // The watcher's transaction already carried them; a batch's
                // sweep still owes them.
                replace_rows: if event.is_some() {
                    None
                } else {
                    Some(projection.rows)
                },
                anchors: projection.anchors,
                ambiguous: projection.ambiguous,
            },
        ));
    }

    let input = chunk_input(&normalized, kind);
    let chunks = chunk_markdown_with(
        source_key,
        &hash,
        &input.text,
        policy,
        input.offset,
        &input.excluded,
        input.rendered_from,
    );
    // The title comes from the indexed text: a `#` inside a frontmatter block is a
    // YAML comment, not a heading.
    let title = derive_title(&input.text, source_key);
    // The parsed block rides in the revision blob (D41.5: unknown fields are
    // Obsidian's data, so the whole block is kept rather than modelled further).
    let metadata_json = revision_metadata(
        &fingerprint,
        &input.frontmatter,
        &input.extracted,
        &input.outline,
    );
    // Vault content is owner content — stated once, used by the revision row and
    // the index document alike, so the two can never disagree.
    let origin = OriginClass::Owner;
    // The rows' `created_at` is this revision's `ingested_at` (ADR 0027): the
    // same number the revision row records, so a rebuild reproduces the rows.
    let ingested_at = now_epoch_secs();
    let projection = project_links(
        &input.extracted,
        source_key,
        from_dir,
        ingested_at,
        ns,
        non_note_keys,
    );

    // One file, one transaction (D40's unit, `commit_source_file`): revision,
    // chunks and edges land together — a crash cannot leave half a file. A
    // batch supplies no event: its coverage row rides the batch-tail sweep
    // (ADR 0026), whose `stale_anchor` is decided against the settled state. A
    // watcher supplies this file's row, so nothing about a file can be
    // recorded before the data it describes (ADR 0026's "lose" half).
    let mut outcome = FileOutcome::Indexed {
        degradations: merge_codes(input.degradations, projection.codes),
        warnings: Vec::new(),
    };
    // `stale_anchor` is decided against the store as this file found it,
    // which for a one-file change *is* the settled state (§5 第 4 条). A
    // batch leaves it to its tail sweep, whose outcome it has not built yet.
    let stale = match mode {
        ProcessMode::Batch => Vec::new(),
        ProcessMode::Watch { .. } => stale_anchors(store, &projection.anchors, non_note_keys)?,
    };
    if !stale.is_empty() {
        outcome.push_degradation(Degradation::StaleAnchor);
    }
    // The payload is only needed where the row rides this transaction: a
    // batch writes its row at the tail and would build the same string a
    // second time, so that arm assigns a placeholder nothing reads.
    let data_json;
    #[allow(unused_assignments)]
    let event = match mode {
        ProcessMode::Batch => {
            data_json = String::new();
            None
        }
        ProcessMode::Watch { batch, .. } => {
            let fields = row_fields(&outcome, raw_name, &projection.ambiguous, &stale);
            data_json = fields.data_json;
            let seq = batch.next_seq();
            Some(coverage_event(
                batch,
                seq,
                Some(source_key),
                fields.outcome,
                fields.reason_code,
                fields.message,
                &data_json,
            ))
        }
    };
    let created = commit_step(source_key, || {
        store.commit_source_file(
            &NewSourceRevision {
                source_key,
                revision_hash: &hash,
                kind: SourceKind::Note,
                title: Some(&title),
                uri: None,
                local_path: Some(&local_path),
                raw_name: Some(raw_name),
                origin_class: origin,
                metadata_json: &metadata_json,
                ingested_at,
            },
            &chunks,
            &projection.rows,
            event.as_ref(),
            guard_against(file, &metadata),
        )
    })?;
    if event.is_some() {
        *row_written = true;
    }

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
    // A source demoted when its file disappeared and brought back re-adopts a
    // row that already exists: no revision was *created*, and counting one
    // would make the report disagree with the table.
    if created {
        report.revisions_created += 1;
    }
    report.chunks_written += chunks.len() as u64;
    // What a file lost never stops the file: its text is indexed and each loss
    // becomes a code on the coverage row (`frontmatter_parse_error`,
    // `canvas_parse_error`, `excalidraw_detected`, and the wikilink codes of
    // `§5 第 7 条`).
    Ok((
        outcome,
        LinkFindings {
            replace_rows: None,
            anchors: projection.anchors,
            ambiguous: projection.ambiguous,
        },
    ))
}

/// One file, with the caller's row timing applied (D40 §4.7 / ADR 0026).
///
/// The body decides what the file *is*; this decides when its row lands. A
/// watcher's row must land beside the data it describes, so the paths that own
/// a transaction write theirs inside the body and set `row_written` — and if
/// such a path fails *after* that commit (the search index refusing the
/// document), no second row is invented for the same file. Everything left —
/// a skip, a refusal — is written here, exactly once, and never in a batch,
/// whose rows all ride the tail sweep.
#[allow(clippy::too_many_arguments)]
fn process_file(
    kind: VaultFileKind,
    store: &mut Store,
    search_index: &SearchIndex,
    writer: &mut IndexWriterHandle,
    workspace_root: &Path,
    file: &Path,
    source_key: &str,
    raw_name: &str,
    policy: &IndexPolicy,
    report: &mut IndexReport,
    ns: &Namespace,
    non_note_keys: &HashSet<String>,
    mode: &mut ProcessMode<'_>,
) -> Result<(FileOutcome, LinkFindings), PassError> {
    let watch = matches!(mode, ProcessMode::Watch { .. });
    let mut row_written = false;
    let passed = process_body(
        kind,
        store,
        search_index,
        writer,
        workspace_root,
        file,
        source_key,
        raw_name,
        policy,
        report,
        ns,
        non_note_keys,
        mode,
        &mut row_written,
    );
    match passed {
        Ok((outcome, findings)) => {
            if watch && !row_written {
                let stale = stale_anchors(store, &findings.anchors, non_note_keys)?;
                let row = PendingRow {
                    source_key: Some(source_key.to_owned()),
                    raw_name: Some(raw_name.to_owned()),
                    outcome: Ok(outcome.clone()),
                    row_written: false,
                    replace_rows: findings.replace_rows.clone(),
                    anchors: findings.anchors.clone(),
                    ambiguous: findings.ambiguous.clone(),
                };
                if let ProcessMode::Watch { batch, .. } = mode {
                    flush_row(store, report, batch, row, stale)?;
                }
                // A projection-only path owes its edges too: the batch's sweep
                // writes them, and a watcher has no sweep.
                if let Some(rows) = findings.replace_rows.clone() {
                    store.replace_edges_many(&[(source_key.to_owned(), rows)])?;
                }
            }
            Ok((outcome, findings))
        }
        Err(PassError::File(error)) => {
            if watch && !row_written {
                let row = refusal_row(Some(source_key), Some(raw_name), error.clone());
                if let ProcessMode::Watch { batch, .. } = mode {
                    flush_row(store, report, batch, row, Vec::new())?;
                }
            }
            Err(PassError::File(error))
        }
        other => other,
    }
}

/// The part of one projection the event sweep still owes work for.
#[derive(Default)]
pub(crate) struct LinkFindings {
    /// Rows to write in the sweep's single batch transaction (`Some` on the
    /// projection-only paths; a parse's rows ride its content transaction).
    replace_rows: Option<Vec<GraphLink>>,
    /// Resolved anchors awaiting validation against settled targets.
    anchors: Vec<(String, AnchorKind, String)>,
    /// Links that matched several notes and built no edge (`§5 第 3 条`).
    ambiguous: Vec<(String, Vec<String>)>,
}

/// Writes one deferred coverage row: `stale_anchor` first (the sweep's whole
/// reason), then the outcome's own codes, plus what the file's name is worth
/// saying (D42).
///
/// One home for the merge, so the placeholder path and the ordinary path
/// cannot disagree about what rides a row: `windows_incompatible_name` (with
/// the components it is about) rides every non-error row — the name is a fact
/// about the file whether or not this run read its bytes, and a report that
/// dropped it on a no-op re-run would call the vault clean.
fn flush_row(
    store: &mut Store,
    report: &mut IndexReport,
    batch: &mut Batch,
    mut row: PendingRow,
    stale: Vec<(String, String)>,
) -> Result<(), RunError> {
    if !stale.is_empty()
        && let Ok(outcome) = &mut row.outcome
    {
        outcome.push_degradation(Degradation::StaleAnchor);
    }
    let (Some(source_key), Some(raw_name)) = (row.source_key, row.raw_name) else {
        let Err(IndexFileError { kind, message, .. }) = row.outcome else {
            // Only error outcomes reach here: every success path carries a
            // raw name. Unreachable by construction; say so rather than
            // invent a row shape for it.
            return Err(RunError::Invariant("a nameless row carried no failure"));
        };
        return record_coverage(
            store,
            report,
            batch,
            None,
            "error",
            None,
            None,
            EMPTY_PAYLOAD,
            Some((kind, message)),
        );
    };
    match row.outcome {
        Err(IndexFileError { kind, message, .. }) => record_coverage(
            store,
            report,
            batch,
            Some(&source_key),
            "error",
            None,
            None,
            EMPTY_PAYLOAD,
            Some((kind, message)),
        ),
        Ok(outcome) => {
            // The primary code and its sentence come from one place, shared
            // with the watcher's in-transaction row (§5 第 8 条 ②⑤).
            let fields = row_fields(&outcome, &raw_name, &row.ambiguous, &stale);
            record_coverage(
                store,
                report,
                batch,
                Some(&source_key),
                fields.outcome,
                fields.reason_code,
                fields.message,
                &fields.data_json,
                None,
            )
        }
    }
}

/// What one file hands the chunker, and what it lost on the way.
struct ChunkInput<'a> {
    /// The parsed `---` block; `Absent` for a canvas, which is not Markdown.
    frontmatter: FrontmatterParse,
    /// The text to chunk: a Markdown note's body slice, or the text a canvas spells
    /// out through its fields.
    text: Cow<'a, str>,
    /// Where `text` starts in the note (zero for a canvas: its text is rendered).
    offset: SourceOffset,
    /// Spans of `text` that must not become chunk text (the Excalidraw scene, §5 第 9 条).
    excluded: Vec<Range<usize>>,
    /// What this file lost, in enum order (§5 第 8 条).
    degradations: Vec<Degradation>,
    /// The file a *rendered* text was built from, when it was not cut out of one (a
    /// readable `.canvas`): its chunk locators then name the file itself.
    rendered_from: Option<&'a str>,
    /// The links this file writes (`§5 第 2 条`): the `links` / `link_losses`
    /// blob keys and the rows the projection builds.
    extracted: Extracted,
    /// Headings and `^id` definitions for later anchors pointing here
    /// (`§5 第 4 条`): the `outline` blob key.
    outline: Outline,
}

/// One file's row, as the four fields every reader of it branches on.
///
/// Shared by `flush_row` (the batch's tail) and by the watcher's
/// in-transaction row: two spellings of "which code leads a row that carries
/// several" is exactly how they drift apart (§5 第 8 条 ②⑤).
struct RowFields {
    /// The outcome word.
    outcome: &'static str,
    /// The stable code a reader branches on, when the file earned one.
    reason_code: Option<&'static str>,
    /// The sentence that goes with it.
    message: Option<&'static str>,
    /// The row's `data_json`, already rendered.
    data_json: String,
}

/// The `data_json` of a row that carries nothing (`payload_json`'s answer for
/// an empty payload, spelled once so no caller builds it by hand).
const EMPTY_PAYLOAD: &str = "{}";

/// Builds a success row's fields from its outcome, its spelling, and what the
/// settled projection still owes it.
///
/// The primary code is the first degradation when the file lost something and
/// the first warning when it did not (§5 第 8 条 ②⑤); the payload carries
/// every list, so a row that lost and earned several things is still counted
/// per code.
fn row_fields(
    outcome: &FileOutcome,
    raw_name: &str,
    ambiguous: &[(String, Vec<String>)],
    stale: &[(String, String)],
) -> RowFields {
    let windows_names = windows_incompatible_components(raw_name);
    let mut warnings = outcome.warnings().to_vec();
    if !windows_names.is_empty() {
        warnings.push(Warning::WindowsIncompatibleName);
    }
    // Enum declaration order, so a re-run reports the same list.
    warnings.sort_unstable();
    warnings.dedup();
    let (reason_code, message) = match (outcome.degradation(), warnings.first().copied()) {
        (Some(code), _) => (
            Some(Degradation::as_str(code)),
            Some(degradation_message(code)),
        ),
        (None, Some(code)) => (Some(Warning::as_str(code)), Some(warning_message(code))),
        (None, None) => (None, None),
    };
    RowFields {
        outcome: outcome.as_str(),
        reason_code,
        message,
        data_json: payload_json(&RowPayload {
            degradations: outcome.degradations(),
            warnings: &warnings,
            windows_names: &windows_names,
            ambiguous,
            stale,
        }),
    }
}

/// One file's coverage row, deferred to the batch's event sweep (ADR 0026:
/// data first, events after).
///
/// The wikilink codes ride here too: `stale_anchor` is only decidable once
/// every file's data — and so every target's outline — is in the store, and a
/// code decided mid-run would depend on walk order (`§5 第 4 条`).
struct PendingRow {
    source_key: Option<String>,
    raw_name: Option<String>,
    outcome: Result<FileOutcome, IndexFileError>,
    /// Whether the row is already in the store (it rode its file's own
    /// transaction, D40 §4.7). The sweep still counts what it owes the batch's
    /// closing event; it just must not append it a second time.
    row_written: bool,
    /// Projection-only rows, written by the sweep's batch (ADR 0027).
    replace_rows: Option<Vec<GraphLink>>,
    /// Resolved anchors awaiting validation: `(target key, kind, anchor text)`.
    anchors: Vec<(String, AnchorKind, String)>,
    /// Links that matched several notes and built no edge (`§5 第 3 条`).
    ambiguous: Vec<(String, Vec<String>)>,
}

/// What one file's links resolve to: the rows to write, the codes to report,
/// and what still needs the settled state (ADR 0027).
struct LinkProjection {
    rows: Vec<GraphLink>,
    codes: Vec<Degradation>,
    anchors: Vec<(String, AnchorKind, String)>,
    ambiguous: Vec<(String, Vec<String>)>,
}

/// The directory a link is written in, in the same vault-relative spelling the
/// namespace matches against.
fn from_dir_for(source_key: &str) -> &str {
    let tail = source_key.strip_prefix("note:").unwrap_or(source_key);
    parent_dir(tail)
}

/// Merges two code lists into the enum's declaration order, once each
/// (§5 第 8 条).
fn merge_codes(mut base: Vec<Degradation>, extra: Vec<Degradation>) -> Vec<Degradation> {
    base.extend(extra);
    base.sort_unstable();
    base.dedup();
    base
}

/// Resolves one extraction against the namespace and builds the rows.
///
/// The rows themselves never depend on anchor validation: a stale anchor keeps
/// its row (`§5 第 4 条`), so the table rebuilds identically whatever the
/// validation state. The codes ride the file's row; `stale_anchor` is added by
/// the event sweep.
fn project_links(
    extracted: &Extracted,
    from_key: &str,
    from_dir: &str,
    created_at: i64,
    ns: &Namespace,
    non_note_keys: &HashSet<String>,
) -> LinkProjection {
    let mut rows = Vec::new();
    let mut codes = extracted.losses.clone();
    let mut anchors = Vec::new();
    let mut ambiguous = Vec::new();
    for link in &extracted.links {
        let decoded_anchor = match (link.link_source, &link.anchor_raw) {
            (momotaro_contracts::LinkSource::Markdown, Some(raw)) => {
                percent_decode(raw).unwrap_or_else(|| raw.clone())
            }
            (_, raw) => raw.clone().unwrap_or_default(),
        };
        match resolve_link(&link.raw_target, from_dir, link.link_source, ns) {
            Resolution::Resolved(target) => {
                // An attachment target takes no anchor parse (§5 第 4 条): the
                // raw fragment is kept, the kind is NULL, and nothing is ever
                // checked against a PDF's page numbers.
                let is_non_note = non_note_keys.contains(&target);
                let anchor_kind = if is_non_note { None } else { link.anchor_kind };
                if let (false, Some(kind)) = (is_non_note, link.anchor_kind) {
                    anchors.push((target.clone(), kind, decoded_anchor));
                }
                rows.push(GraphLink {
                    from_source_key: from_key.to_owned(),
                    to_source_key: Some(target),
                    raw_target: link.raw_target.clone(),
                    edge_kind: link.edge_kind,
                    link_source: link.link_source,
                    anchor_kind,
                    anchor_raw: link.anchor_raw.clone(),
                    created_at,
                });
            }
            Resolution::Dangling => {
                codes.push(Degradation::UnresolvedWikilink);
                rows.push(GraphLink {
                    from_source_key: from_key.to_owned(),
                    to_source_key: None,
                    raw_target: link.raw_target.clone(),
                    edge_kind: link.edge_kind,
                    link_source: link.link_source,
                    anchor_kind: link.anchor_kind,
                    anchor_raw: link.anchor_raw.clone(),
                    created_at,
                });
            }
            Resolution::Ambiguous(candidates) => {
                codes.push(Degradation::AmbiguousWikilink);
                ambiguous.push((link.raw_target.clone(), candidates));
            }
        }
    }
    codes.sort_unstable();
    codes.dedup();
    LinkProjection {
        rows,
        codes,
        anchors,
        ambiguous,
    }
}

/// Reads the `links` / `link_losses` keys back out of a revision blob.
///
/// Read stances differ by surface, on purpose: the `graph_links` **table** is
/// typed at both ends and refuses an unknown spelling with
/// `StoreError::UnknownGraphSpelling`; this **blob** is a cache of raw
/// extractions, and a record spelled by a newer build is skipped without a
/// panic. A blob from before the keys existed reads as no links — its files
/// rebuild their edges when their content next changes (§5 第 2 条 迁移注记).
fn parse_link_blob(metadata_json: &str) -> Extracted {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(metadata_json) else {
        return Extracted::default();
    };
    let mut out = Extracted::default();
    if let Some(records) = value.get("links").and_then(serde_json::Value::as_array) {
        for record in records {
            let Some(object) = record.as_object() else {
                continue;
            };
            let (Some(raw_target), Some(edge_kind), Some(link_source)) = (
                object.get("raw_target").and_then(serde_json::Value::as_str),
                object
                    .get("edge_kind")
                    .and_then(serde_json::Value::as_str)
                    .and_then(momotaro_contracts::EdgeKind::from_code),
                object
                    .get("link_source")
                    .and_then(serde_json::Value::as_str)
                    .and_then(momotaro_contracts::LinkSource::from_code),
            ) else {
                // A record this build cannot spell is a link it cannot speak
                // for: the row is dropped, but the loss is reported — the
                // audit promise of §5 第 7 条 covers downgrade windows too.
                out.push_loss(Degradation::UnknownLinkSyntax);
                continue;
            };
            // An unknown spelling in any vocabulary skips the record whole —
            // one stance, like `edge_kind`/`link_source` above: a record with a
            // rewritten anchor is not the record that was written.
            let anchor_kind = match object.get("anchor_kind") {
                None | Some(serde_json::Value::Null) => None,
                Some(value) => match value.as_str().and_then(AnchorKind::from_code) {
                    Some(kind) => Some(kind),
                    None => {
                        out.push_loss(Degradation::UnknownLinkSyntax);
                        continue;
                    }
                },
            };
            out.push_link(momotaro_ingest::RawLink {
                raw_target: raw_target.to_owned(),
                edge_kind,
                link_source,
                anchor_kind,
                anchor_raw: object
                    .get("anchor_raw")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            });
        }
    }
    if let Some(losses) = value
        .get("link_losses")
        .and_then(serde_json::Value::as_array)
    {
        for code in losses {
            if let Some(code) = code.as_str().and_then(Degradation::from_code) {
                out.push_loss(code);
            }
        }
    }
    out
}

/// The target-side outline stored in a revision blob (`§5 第 4 条`).
///
/// `None` when the key is absent (a blob older than the key): an anchor
/// pointing here is not verifiable, and "cannot verify" is not "missing" —
/// only a confirmed miss reports `stale_anchor`.
fn outline_from_blob(metadata_json: &str) -> Option<Outline> {
    let value: serde_json::Value = serde_json::from_str(metadata_json).ok()?;
    let outline = value.get("outline")?;
    let mut out = Outline::default();
    for path in outline
        .get("headings")
        .and_then(serde_json::Value::as_array)?
    {
        // A malformed entry makes the whole outline unverifiable: a silently
        // truncated path could match an anchor that was never written.
        let levels = path
            .as_array()?
            .iter()
            .map(|level| level.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?;
        out.headings.push(levels);
    }
    for id in outline
        .get("block_ids")
        .and_then(serde_json::Value::as_array)?
    {
        out.block_ids.push(id.as_str()?.to_owned());
    }
    Some(out)
}

/// Every anchor that was checked and found missing in its target, as
/// `(target key, anchor)` — the audit trail a `stale_anchor` row carries
/// (`data_json`), symmetric with the ambiguous candidates.
///
/// Runs in the event sweep, against the state every file's data left behind.
/// Attachment and canvas targets are never checked (`§5 第 4 条`), and a note
/// whose blob predates `outline` cannot be checked — neither reports a code.
fn stale_anchors(
    store: &Store,
    checks: &[(String, AnchorKind, String)],
    non_note_keys: &HashSet<String>,
) -> Result<Vec<(String, String)>, RunError> {
    let mut missing = Vec::new();
    // One lookup per target, not per anchor: a note with ten links to one
    // target checks its outline once.
    let mut outlines: HashMap<String, Option<Outline>> = HashMap::new();
    for (target, kind, raw) in checks {
        if non_note_keys.contains(target) {
            continue;
        }
        if !outlines.contains_key(target) {
            // A store refusal is not "cannot verify": it propagates (store_step's
            // stance), or a transient error would silently publish clean rows.
            let outline = match store.current_revision(target)? {
                Some(revision) if revision.kind == SourceKind::Note => {
                    outline_from_blob(&revision.metadata_json)
                }
                _ => None,
            };
            outlines.insert(target.clone(), outline);
        }
        // `None` covers "no revision", "not a note", and "no outline": none of
        // them is a checked-and-missing anchor.
        let Some(outline) = outlines[target].as_ref() else {
            continue;
        };
        if !anchor_still_exists(*kind, raw, outline) {
            missing.push((target.clone(), raw.clone()));
        }
    }
    Ok(missing)
}

/// Derives the chunker's input, and the code list that goes with it.
///
/// One derivation for both paths (indexing and refresh): a file that lost
/// something must not report clean on one of them.
fn chunk_input<'a>(normalized: &'a str, kind: VaultFileKind) -> ChunkInput<'a> {
    // A `.canvas` is not Markdown — no `---` block, no headings — and the text it
    // contributes is the one JSON Canvas assigns a meaning to (§5 第 9 条). Text that
    // cannot be read as a canvas object keeps the file's own text as the body and
    // reports the code, so the file is never silently empty.
    if kind == VaultFileKind::Canvas {
        return match canvas_text(normalized) {
            Some(text) => ChunkInput {
                frontmatter: FrontmatterParse::Absent,
                text: Cow::Owned(text),
                offset: SourceOffset::default(),
                excluded: Vec::new(),
                degradations: Vec::new(),
                rendered_from: Some(normalized),
                // Canvas edge building is its own task (B7 / §5 第 2 条): a
                // canvas contributes no extraction yet, and none is stored.
                extracted: Extracted::default(),
                outline: Outline::default(),
            },
            None => ChunkInput {
                frontmatter: FrontmatterParse::Absent,
                text: Cow::Borrowed(normalized),
                offset: SourceOffset::default(),
                excluded: Vec::new(),
                degradations: vec![Degradation::CanvasParseError],
                rendered_from: None,
                extracted: Extracted::default(),
                outline: Outline::default(),
            },
        };
    }

    let (frontmatter, body) = frontmatter::split(normalized);
    let excluded = scene_spans(body);
    let mut degradations = degradations_of(&frontmatter);
    if !excluded.is_empty() {
        degradations.push(Degradation::ExcalidrawDetected);
    }
    // One extraction for both consumers (rows and blob): body links minus the
    // excluded scene, plus the frontmatter block's own links from its `raw`
    // (§5 第 5 条: an unmodelled field shape cannot hide a link).
    // Frontmatter first, then the body: record order is the note's document
    // order (§5 第 2 条), and first occurrence wins the identity dedupe.
    let mut extracted = extract_frontmatter_links(frontmatter_raw(&frontmatter).unwrap_or(""));
    let (body_links, outline) = analyze(body, &excluded);
    extracted.absorb(body_links);
    ChunkInput {
        frontmatter,
        text: Cow::Borrowed(body),
        offset: SourceOffset::before_body(normalized, body),
        excluded,
        degradations,
        rendered_from: None,
        outline,
        extracted,
    }
}

/// The block's raw text, on every path that kept it (`Parsed`, `Json`,
/// `Failed`) — the one thing link extraction reads (§5 第 5 条).
fn frontmatter_raw(parse: &FrontmatterParse) -> Option<&str> {
    match parse {
        FrontmatterParse::Absent => None,
        FrontmatterParse::Parsed(frontmatter) => Some(&frontmatter.raw),
        FrontmatterParse::Json(raw) | FrontmatterParse::Failed { raw, .. } => Some(raw),
    }
}

/// The text a JSON Canvas contributes, or `None` when the file is not one.
///
/// Only the fields the spec gives a text meaning become text (§5 第 9 条): `text` nodes
/// verbatim (the spec says it is Markdown), `file` nodes with their `subpath`
/// appended, `link` nodes by URL, `group` nodes by `label` when they have one — and
/// then one `<from text> -> <to text>` line per edge. Nodes keep their array order
/// (the spec's own: array order is the z-index), `id`/geometry/colors never become
/// text, and a node whose type this build does not know contributes nothing while
/// the rest of the file is still indexed. `Some("")` is a canvas with nothing in
/// it: zero segments, not a failure.
fn canvas_text(raw: &str) -> Option<String> {
    let serde_json::Value::Object(root) = serde_json::from_str::<serde_json::Value>(raw).ok()?
    else {
        return None;
    };
    let mut blocks: Vec<String> = Vec::new();
    // Edge ends are named by id; the id is unique only inside this canvas and is
    // never text, so it is used here and nowhere else.
    let mut named: HashMap<&str, usize> = HashMap::new();
    // A `nodes` that is present but is not an array is not a canvas either: reading
    // it as "nothing to index" would make an unreadable file look like an empty
    // one, and it is the failure path that keeps a file from going silently empty.
    let nodes: &[serde_json::Value] = match root.get("nodes") {
        Some(value) => value.as_array()?.as_slice(),
        None => &[],
    };
    {
        for node in nodes {
            let Some(node) = node.as_object() else {
                continue;
            };
            let text = match node.get("type").and_then(serde_json::Value::as_str) {
                Some("text") => string_field(node, "text"),
                Some("file") => {
                    string_field(node, "file").map(|file| match string_field(node, "subpath") {
                        Some(subpath) => format!("{file}{subpath}"),
                        None => file,
                    })
                }
                Some("link") => string_field(node, "url"),
                // A group without a label has no text of its own: an empty group is
                // normal, and a placeholder would be a word we invented.
                Some("group") => string_field(node, "label"),
                _ => None,
            };
            let Some(text) = text else {
                continue;
            };
            if let Some(id) = node.get("id").and_then(serde_json::Value::as_str) {
                named.insert(id, blocks.len());
            }
            blocks.push(text);
        }
    }
    let edges: &[serde_json::Value] = match root.get("edges") {
        Some(value) => value.as_array()?.as_slice(),
        None => &[],
    };
    {
        for edge in edges {
            let Some(edge) = edge.as_object() else {
                continue;
            };
            let (Some(from), Some(to)) = (
                edge.get("fromNode").and_then(serde_json::Value::as_str),
                edge.get("toNode").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let mut line = format!(
                "{} -> {}",
                endpoint(from, &named, &blocks),
                endpoint(to, &named, &blocks)
            );
            if let Some(label) = string_field(edge, "label") {
                line.push_str(": ");
                line.push_str(&label);
            }
            blocks.push(line);
        }
    }
    Some(blocks.join("\n\n"))
}

/// A string field of a JSON object, if it is there and is a string.
fn string_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// What an edge names: the text its end contributed, or the id as written when
/// that node contributed no text (a dangling end is kept, never dropped).
fn endpoint<'a>(id: &'a str, named: &HashMap<&str, usize>, blocks: &'a [String]) -> &'a str {
    named.get(id).map_or(id, |index| blocks[*index].as_str())
}

/// Registers a vault file the index does not read: one revision, no chunks.
///
/// The file is a source — it has a key, a revision and a coverage row — and its
/// bytes are deliberately not searchable, which is what the row's
/// `attachment_unindexed` says (§5 第 10 条). Two things it must not do: run
/// `normalize_markdown` over binary (that is Markdown's BOM/CRLF pass), and read
/// the file into memory to hash it — a vault may hold a video, so the hash
/// streams and the file's own bytes are never held whole.
fn register_attachment(
    store: &mut Store,
    workspace_root: &Path,
    file: &Path,
    source_key: &str,
    raw_name: &str,
    report: &mut IndexReport,
) -> Result<FileOutcome, IndexFileError> {
    let metadata = fs::metadata(file)
        .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?;
    let fingerprint = fingerprint_json(&metadata);
    let local_path = momotaro_ingest::local_path_in(workspace_root, file)
        .map_err(|error| file_error(Some(source_key), error.kind(), error))?;

    let current = store_step(source_key, || store.current_revision(source_key))?;
    if let Some(revision) = &current
        && fingerprint_matches(&revision.metadata_json, &metadata)
        && revision.local_path.as_deref() == Some(local_path.as_str())
        && revision.raw_name.as_deref() == Some(raw_name)
    {
        report.files_skipped += 1;
        return Ok(FileOutcome::Skipped {
            degradations: vec![Degradation::AttachmentUnindexed],
            warnings: Vec::new(),
        });
    }

    let hash = {
        let mut reader = fs::File::open(file)
            .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?;
        revision_hash_reader(&mut reader)
            .map_err(|error| file_error(Some(source_key), IndexFileErrorKind::Io, error))?
    };

    // Same bytes under a fresh fingerprint (a `touch`, a copy, a sync that rewrote
    // the mtime): the revision already holds them, so only the mutable columns move.
    // Without this branch every such run appended a revision, reported one created
    // and left the store with a single row — the count and the truth parted ways,
    // which is the shape `process_file` handles for notes for the same reason.
    if let Some(revision) = &current
        && revision.revision_hash == hash
    {
        store_step(source_key, || {
            store.update_revision_fingerprint(
                source_key,
                &revision.revision_hash,
                &fingerprint,
                &local_path,
                raw_name,
            )
        })?;
        report.files_indexed += 1;
        return Ok(FileOutcome::MetadataRefreshed {
            degradations: vec![Degradation::AttachmentUnindexed],
            warnings: Vec::new(),
        });
    }

    // No H1 to find and no frontmatter to record: the fingerprint is the whole
    // blob, and the title is the file's name without its extension. The unit
    // carries no chunks and no edges — the kind is decided by the extension, so
    // a key that is an attachment never had either.
    let title = derive_title("", source_key);
    let created = store_step(source_key, || {
        store.commit_source_file(
            &NewSourceRevision {
                source_key,
                revision_hash: &hash,
                kind: SourceKind::Attachment,
                title: Some(&title),
                uri: None,
                local_path: Some(&local_path),
                raw_name: Some(raw_name),
                origin_class: OriginClass::Owner,
                metadata_json: &fingerprint,
                ingested_at: now_epoch_secs(),
            },
            &[],
            &[],
            None,
            || Ok(()),
        )
    })?;

    report.files_indexed += 1;
    // See the note path: a restored attachment re-adopts its own row.
    if created {
        report.revisions_created += 1;
    }
    Ok(FileOutcome::Indexed {
        degradations: vec![Degradation::AttachmentUnindexed],
        warnings: Vec::new(),
    })
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
