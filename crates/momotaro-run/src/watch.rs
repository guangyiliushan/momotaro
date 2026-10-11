//! The vault watcher: events are hints, reconciliation is the truth.
//!
//! Everything a platform watcher reports is treated the same way — as a hint
//! that one path may deserve a look. The hint is never the input to
//! a write: a path is gated with a stat and, when that stat is racy, with the
//! file's own bytes, and only then does anything reach the store. The four
//! reconciliation triggers are what turns an observation the event
//! stream could not carry — a dropped event, a storm, a file changed while the
//! process was down — back into the truth.
//!
//! # What this module has no instrument for
//!
//! Two of the cases below cannot be produced on any CI machine, and no test here
//! pretends otherwise:
//!
//! * **A real platform overflow.** Linux `IN_Q_OVERFLOW` and FSEvents
//!   `KernelDropped` surface as notify's `Flag::Rescan`, which is a plain value
//!   a test can build — that half *is* covered. Windows'
//!   `ReadDirectoryChangesW` overflow surfaces as nothing at all in `notify`
//!   8.2.0 (the fix ships only in 9.0.0-rc.5), so there is **no out-of-band
//!   instrument** for it: the gap is closed by the daily and startup
//!   reconciliations, and said here.
//! * **A real network volume**, whose event delivery we promise nothing about
//!   The verdict comes from `momotaro_platform`, whose own tests
//!   pin the label rule; an end-to-end case is a hand-run probe.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use momotaro_ingest::{attributes_of, source_key_for};
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{PollWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, RecommendedCache, new_debouncer, new_debouncer_opt,
};
use serde::Serialize;

use momotaro_store::{NewStoreEvent, Store};

use crate::index_service::{BatchOutcome, BatchShape, WATCH_PLAN, run_batch};
use crate::reconcile::reconcile;
use crate::{NETWORK_VOLUME_NOTE, RunError, database_path, load_config, vault_scope};

/// How long a path's events are held before they are acted on.
///
/// `notify-debouncer-full` picks its own tick at a quarter of this unless told
/// otherwise, which is what keeps a burst of saves to a single pass.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// One debounce batch naming more paths than this is an event storm,
/// and is answered with a full reconciliation rather than N rescans.
///
/// A threshold, not a measurement — nothing in notify's contract describes
/// one. It sits where a single pass over the named paths would cost about what
/// a reconciliation costs, and it is a constant so a test can pin both sides.
const STORM_PATHS: usize = 256;

/// How long a reconcile waits for its cookie before carrying on.
///
/// Watchman errors out when its cookie does not come back; a reconcile here
/// does not. Waiting is an optimisation — walking without it is still correct,
/// just one event behind — and a non-destructive command must not fail because
/// a listener was slow.
const COOKIE_TIMEOUT: Duration = Duration::from_secs(5);

/// The delay queue's wait, after the five backoffs.
const TORN_DELAY: Duration = Duration::from_secs(30);

/// The mtime has to hold still for at least this long between two
/// stats before a torn read is retried, so a writer mid-flush cannot look
/// stable for an instant and then move again.
const MTIME_STABLE_GAP: Duration = Duration::from_millis(64);

/// The daily trigger: once a day, from inside the running watcher.
const DAILY: Duration = Duration::from_secs(24 * 60 * 60);

/// The sentinel file's name, in the vault root.
///
/// Dot-prefixed so the walker never indexes it and the blacklist never
/// rescans it — and recognised *before* the blacklist, which is what makes it
/// a protocol message rather than a file.
const SENTINEL_NAME: &str = ".momotaro-reconcile-sentinel";

/// Dropped at the event layer, before any filesystem question.
///
/// Two rules, both the walker's own: anything under a
/// dot-prefixed directory, and any dot-prefixed file — except an iCloud
/// eviction stub, which the walker keeps precisely so a row can say what
/// happened to the note it stands for. On top of those sit the five
/// editor-temporary shapes: a single `index` walk meets them once,
/// a live watcher would otherwise chase them on every keystroke.
///
/// The sentinel is dot-prefixed like every other leaf here, so it *is*
/// dropped — by this rule, not by an exemption. What finds it is the cookie
/// scan in `run_reconcile`, which reads the raw event stream before any name
/// rule is applied.
#[must_use]
fn blacklisted(vault_root: &Path, path: &Path) -> bool {
    let Ok(tail) = path.strip_prefix(vault_root) else {
        // Not inside the vault at all: nothing here is ours to scan.
        return true;
    };
    let mut components = tail.components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str();
        let leaf = components.peek().is_none();
        if momotaro_ingest::is_hidden(name) {
            // A dot-prefixed directory drops its whole subtree. A dot-prefixed
            // file drops too — except an iCloud eviction stub, which the walker
            // keeps precisely so a row can say what happened to the note it
            // stands for: dropping it here would be the two-truths case
            // that must not happen.
            let stub = leaf && name.to_str().is_some_and(momotaro_ingest::icloud_stub);
            if !stub {
                return true;
            }
        }
        if leaf && editor_temporary(&name.to_string_lossy()) {
            return true;
        }
    }
    false
}

/// One of the five editor-temporary shapes, on a leaf name only.
///
/// `.~lock.*#` is dot-prefixed and so already covered by the rule above; it is
/// spelled out anyway, and a reader checking this list
/// against the contract should find every entry.
fn editor_temporary(name: &str) -> bool {
    (name.starts_with(".~lock.") && name.ends_with('#'))
        || name.starts_with("~$")
        || name.ends_with(".swp")
        || name.ends_with('~')
        || name.ends_with(".tmp")
}

/// Which of the four triggers fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The watcher started.
    Startup,
    /// The daily timer.
    Daily,
    /// notify reported that it dropped events.
    DroppedEvents,
    /// One batch named too many paths.
    EventStorm,
}

/// What a debounced batch of events amounts to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WatchAction {
    /// Nothing survived the blacklist: the events were about junk.
    Nothing,
    /// The platform says it dropped events, or the batch was a storm. Only
    /// these two ever come from an event batch; `Startup` and `Daily` are the
    /// loop's own.
    Reconcile(Trigger),
    /// These paths deserve a look. A rename arrives as its two paths, which is
    /// the delete+create pair already folded.
    Rescan(Vec<PathBuf>),
}

/// Turns one debounced batch into what the watcher should do with it.
///
/// Pure and total: no filesystem question is asked here, because the blacklist
/// is a name rule, and the rescan is where facts get established.
#[must_use]
fn actions_for(events: &[DebouncedEvent], vault_root: &Path) -> WatchAction {
    if events.iter().any(|debounced| debounced.event.need_rescan()) {
        return WatchAction::Reconcile(Trigger::DroppedEvents);
    }
    // A set for membership and a vec for order, and the threshold checked as
    // the set grows: a batch that is already a storm never pays for the rest
    // of it, and dedup stops being quadratic on the way there.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for debounced in events {
        for path in &debounced.event.paths {
            if blacklisted(vault_root, path) {
                continue;
            }
            if seen.insert(path.clone()) {
                if paths.len() + 1 > STORM_PATHS {
                    return WatchAction::Reconcile(Trigger::EventStorm);
                }
                paths.push(path.clone());
            }
        }
    }
    if paths.is_empty() {
        return WatchAction::Nothing;
    }
    WatchAction::Rescan(paths)
}

/// How long to wait before retry number `failures + 1`, or `None`
/// when the delay queue has been served and the path belongs to the daily
/// reconciliation.
///
/// Pure so the schedule can be pinned without sleeping: 100ms, 200ms, 400ms,
/// 800ms, 1600ms — five attempts — then the 30s delay queue, then give up.
#[must_use]
fn retry_delay(failures: u32) -> Option<Duration> {
    match failures {
        0..=4 => Some(Duration::from_millis(100_u64 << failures)),
        5 => Some(TORN_DELAY),
        _ => None,
    }
}

/// Whether `path` has held still for [`MTIME_STABLE_GAP`].
///
/// Two stats that far apart and identical say the writer has stopped; anything
/// else means the file is still moving and reading it now would tear again.
fn mtime_stable(path: &Path) -> std::io::Result<bool> {
    let first = std::fs::metadata(path)?;
    thread::sleep(MTIME_STABLE_GAP);
    let second = std::fs::metadata(path)?;
    Ok(same_stat(&first, &second))
}

/// Whether two stats describe the same file state — the rule `mtime_stable`
/// applies to the two it takes.
///
/// Both halves are load-bearing: size alone misses a rewrite of equal length,
/// and mtime alone misses a filesystem that stamps only seconds. Pinned by
/// tests that need neither the sleep nor a racing writer.
///
/// Crate-visible because `index_service`'s tornness check is the same question
/// asked about a re-read, and a second copy of it was how the two drifted.
pub(crate) fn same_stat(first: &std::fs::Metadata, second: &std::fs::Metadata) -> bool {
    same_state(
        (first.len(), first.modified().ok()),
        (second.len(), second.modified().ok()),
    )
}

/// Whether two stat states describe the same file (the stable window).
///
/// Values rather than `&Metadata`, because both halves have to be pinned and
/// no test can change a file's length while leaving its mtime alone: size
/// alone misses a same-length rewrite, mtime alone misses a same-tick rewrite
/// of a different length.
fn same_state(
    first: (u64, Option<std::time::SystemTime>),
    second: (u64, Option<std::time::SystemTime>),
) -> bool {
    first.0 == second.0 && first.1 == second.1
}

/// One line of what the watcher is doing, for whatever is rendering it.
///
/// Both exits the contract promises live here: the CLI prints this as JSON
/// under `--json` and as a human sentence otherwise, so a field the docs name
/// cannot be present in one and absent from the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchLine {
    /// A rescan batch finished.
    Batch {
        /// The `ingest.*` batch these rows belong to.
        batch_id: String,
        /// Paths the gate let through.
        ///
        /// A path whose key the batch finds gone still spends a scan here —
        /// the walk resolves it, the compare skips it (`unchanged`) and the
        /// batch records it in `removed`.
        rescanned: u64,
        /// Files whose bytes changed.
        rebuilt: u64,
        /// Files whose bytes were unchanged but whose fingerprint, path or
        /// spelling moved — counted separately because it is neither of the
        /// two above and a reader that only saw `rebuilt` and `unchanged`
        /// would find a row with no home (`doctor --coverage` calls it
        /// `refreshed`).
        refreshed: u64,
        /// Files that turned out to need nothing.
        ///
        /// A deletion lands here too: nothing was rebuilt for a file that is
        /// gone, and the removal itself is named by `removed`.
        unchanged: u64,
        /// Keys the walk no longer names, which this batch recorded as gone.
        removed: u64,
        /// Paths the gate threw away — named paths the batch never processed
        /// at all.
        ///
        /// A deletion is *not* one of them: a deleted file the batch heard
        /// about was let through, spent its scan and is counted in
        /// `rescanned`/`unchanged`/`removed` — measured `dropped=0` on a
        /// one-deletion batch. Every named path is one or the other
        /// (`rescanned + dropped` names them all), never both.
        dropped: u64,
    },
    /// The recommended backend would not start and polling took over.
    Degraded {
        /// What failed, in the platform's own words.
        reason: String,
    },
    /// The vault sits on a network volume, which is not promised to be
    /// real-time. The sentence rides the line itself so the
    /// JSON exit carries it too — `--json` and the human line must not
    /// promise different things.
    NetworkVolume {
        /// The shared wording: no real-time promise, and why it matters.
        message: String,
    },
    /// The reconcile cookie could not be used, so the walk ran without it
    /// still correct, just possibly one event behind. `reason` says
    /// whether the sentinel refused to be written or simply never came back.
    CookieSkipped {
        /// Why the cookie was not established.
        reason: String,
    },
    /// A path ran out of retry attempts — a torn read that would not settle,
    /// or a file that kept moving under the commit — and was left to the daily
    /// reconciliation.
    RetriesExhausted {
        /// The path that gave up.
        path: String,
    },
    /// A reconciliation ran, and why.
    Reconciled {
        /// Which of the four triggers fired.
        trigger: Trigger,
        /// The `vault.reconcile.*` batch it wrote.
        batch_id: String,
    },
}

/// One [`WatchLine`] as a sentence, for a terminal.
///
/// The only place these words are written: the JSON exit carries the same
/// fields, so a promise the docs make about one is a promise about the other.
/// Anything a file name put here is sanitized first — a name can hold a
/// control character and this goes straight to stdout.
pub fn format_watch_line(line: &WatchLine) -> String {
    match line {
        WatchLine::Batch {
            batch_id,
            rescanned,
            rebuilt,
            refreshed,
            unchanged,
            removed,
            dropped,
        } => {
            let extra = |count: &u64, word: &str| {
                if *count > 0 {
                    format!(", {count} {word}")
                } else {
                    String::new()
                }
            };
            format!(
                "ingest batch {batch_id}: {rescanned} rescanned, {rebuilt} rebuilt{refreshed}{unchanged}{removed}{dropped}",
                refreshed = extra(refreshed, "refreshed"),
                unchanged = extra(unchanged, "unchanged"),
                removed = extra(removed, "removed"),
                dropped = extra(dropped, "dropped"),
            )
        }
        WatchLine::Degraded { reason } => format!(
            "falling back to polling, so updates are no longer real-time: {}",
            crate::sanitize(reason)
        ),
        WatchLine::NetworkVolume { message } => crate::sanitize(message),
        WatchLine::CookieSkipped { reason } => format!(
            "the reconcile cookie was not established ({reason}); the walk ran without it",
            reason = crate::sanitize(reason)
        ),
        WatchLine::RetriesExhausted { path } => format!(
            "{} could not be read or committed after several attempts; the next reconcile will pick it up",
            crate::sanitize(path)
        ),
        WatchLine::Reconciled { trigger, batch_id } => format!(
            "reconciled ({}): batch {batch_id}",
            match trigger {
                Trigger::Startup => "watcher startup",
                Trigger::Daily => "daily",
                Trigger::DroppedEvents => "the platform dropped events",
                Trigger::EventStorm => "event storm",
            }
        ),
    }
}

/// A path that could not be read, and what the retry schedule owes it.
#[derive(Debug, Clone)]
struct Retry {
    path: PathBuf,
    /// Failures so far; [`retry_delay`] decides what comes next.
    failures: u32,
    when: Instant,
}

/// The two backends this module allows, so the fallback does not make every
/// caller generic over `notify`'s watcher types.
///
/// The payload is never *read*: it exists to own the debouncer for as long as
/// the loop runs, because dropping one stops its thread. That is the whole
/// reason this is an enum rather than two locals.
#[allow(dead_code)]
enum Backend {
    /// The platform's own: inotify, FSEvents, `ReadDirectoryChangesW`.
    Recommended(notify_debouncer_full::Debouncer<notify::RecommendedWatcher, RecommendedCache>),
    /// The fallback, used only when the recommended one would not start — and
    /// never for a network volume, which is never pretended to be supported.
    Polling(notify_debouncer_full::Debouncer<PollWatcher, RecommendedCache>),
}

/// Opens the backend: the platform's own if it starts,
/// polling if it does not — and neither, for a network volume.
///
/// The receiver comes back with it because only the successful attempt owns
/// the channel the loop drains.
fn open_backend(
    vault_root: &Path,
    network: bool,
    emit: &mut dyn FnMut(&WatchLine),
) -> Result<(Backend, mpsc::Receiver<DebounceEventResult>), RunError> {
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut failure: Option<String> = None;
    let mut recommended = None;
    match new_debouncer(DEBOUNCE, None, tx) {
        Ok(mut debouncer) => match debouncer.watch(vault_root, RecursiveMode::Recursive) {
            Ok(()) => recommended = Some(Backend::Recommended(debouncer)),
            Err(reason) => failure = Some(reason.to_string()),
        },
        Err(error) => failure = Some(error.to_string()),
    }
    if let Some(backend) = recommended {
        return Ok((backend, rx));
    }

    let reason = failure.unwrap_or_else(|| "the filesystem backend did not start".to_owned());
    if network {
        // No pretending: the network-volume warning has already been
        // emitted; this is the backend refusing as well.
        return Err(RunError::WatcherUnavailable {
            reason: format!("{reason}; the vault is on a network volume, which is not supported"),
        });
    }
    // notify's own words, which can embed a path: sanitized on the way in, so
    // both exits carry the same safe string (`--json` does not escape a
    // bidi override).
    emit(&WatchLine::Degraded {
        reason: crate::sanitize(&reason),
    });
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut polling = new_debouncer_opt::<_, PollWatcher, _>(
        DEBOUNCE,
        None,
        tx,
        RecommendedCache::new(),
        notify::Config::default(),
    )
    .map_err(|error| RunError::WatcherUnavailable {
        reason: error.to_string(),
    })?;
    polling
        .watch(vault_root, RecursiveMode::Recursive)
        .map_err(|error| RunError::WatcherUnavailable {
            reason: error.to_string(),
        })?;
    Ok((Backend::Polling(polling), rx))
}

/// Run the watcher until the process stops.
///
/// The backend is opened *before* the startup reconciliation, so the event
/// stream is live when the walk begins — that is what makes the cookie
/// mean anything. `emit` is called once per thing that happened and is the
/// only way anything reaches a terminal.
///
/// # Errors
///
/// - `RunError::ConfigRead` / `RunError::ConfigParse` when `momotaro.toml`
///   cannot be read or parsed.
/// - `RunError::VaultOutsideWorkspace` when the configured vault resolves
///   outside the workspace.
/// - `RunError::VaultMissing` when the resolved vault path is not a
///   directory.
/// - `RunError::WatcherUnavailable` when neither the recommended nor the
///   polling filesystem backend can watch the vault.
/// - `RunError::Ingest` when a vault path check or walk fails.
/// - What a batch surfaces from the startup/daily reconciliation and the
///   per-event rescan: `RunError::IndexUnusable`,
///   `RunError::StoreUnhealthy`, `RunError::Store`, `RunError::Retrieve`,
///   `RunError::Io` and `RunError::Invariant`, plus the config and vault
///   re-checks each batch performs. Transient store/IO failures on the
///   per-event path are reported as degraded lines and retried, not
///   returned.
pub fn watch(root: impl AsRef<Path>, emit: &mut dyn FnMut(&WatchLine)) -> Result<(), RunError> {
    let root = root.as_ref();
    let config = load_config(root)?;
    let vault_root = vault_scope(root, &config)?.root;
    if !vault_root.is_dir() {
        return Err(RunError::VaultMissing { path: vault_root });
    }
    let sentinel = vault_root.join(SENTINEL_NAME);
    let network = momotaro_platform::volume_info(&vault_root).remote == Some(true);
    // This is said out loud rather than discovered later as "my edits
    // never show up". Detection is Windows-only: every other platform reports
    // "unknown" and this stays silent rather than guessing.
    if network {
        emit(&WatchLine::NetworkVolume {
            message: NETWORK_VOLUME_NOTE.to_owned(),
        });
    }

    // Held for its `Drop`, which stops the debouncer's thread.
    let (_backend, rx) = open_backend(&vault_root, network, emit)?;
    let mut retries: Vec<Retry> = Vec::new();
    // Trigger 1, with the cookie.
    run_reconcile(
        root,
        &vault_root,
        &sentinel,
        &rx,
        Trigger::Startup,
        emit,
        &mut retries,
    )?;

    let mut daily = Instant::now() + DAILY;
    loop {
        let now = Instant::now();
        let mut timeout = daily.saturating_duration_since(now);
        for retry in &retries {
            timeout = timeout.min(retry.when.saturating_duration_since(now));
        }
        // Bounded so the daily timer and a due retry are never more than a
        // tick late even when no event ever arrives.
        let timeout = timeout.clamp(Duration::from_millis(10), Duration::from_millis(250));
        match rx.recv_timeout(timeout) {
            // A debouncer error means notify could not deliver: the daily
            // reconciliation is what covers it.
            Ok(Ok(events)) => apply(
                root,
                &vault_root,
                &sentinel,
                &rx,
                &events,
                emit,
                &mut retries,
            )?,
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }

        if Instant::now() >= daily {
            daily = Instant::now() + DAILY;
            run_reconcile(
                root,
                &vault_root,
                &sentinel,
                &rx,
                Trigger::Daily,
                emit,
                &mut retries,
            )?;
        }
        drain_retries(root, &mut retries, emit)?;
    }
}

/// Applies one debounced batch.
fn apply(
    root: &Path,
    vault_root: &Path,
    sentinel: &Path,
    rx: &mpsc::Receiver<DebounceEventResult>,
    events: &[DebouncedEvent],
    emit: &mut dyn FnMut(&WatchLine),
    retries: &mut Vec<Retry>,
) -> Result<(), RunError> {
    match actions_for(events, vault_root) {
        WatchAction::Nothing => Ok(()),
        WatchAction::Reconcile(trigger) => {
            run_reconcile(root, vault_root, sentinel, rx, trigger, emit, retries)
        }
        WatchAction::Rescan(paths) => {
            // A transient store/fs failure must not kill the daemon. Requeue
            // the same hints through the existing per-file backoff.
            let outcome = match rescan(root, paths.clone(), emit) {
                Ok(outcome) => outcome,
                Err(error) if transient_watch_error(&error) => {
                    emit(&WatchLine::Degraded {
                        reason: crate::sanitize(&error.to_string()),
                    });
                    BatchOutcome {
                        report: momotaro_contracts::IndexReport {
                            batch_id: String::new(),
                            files_scanned: 0,
                            files_skipped: 0,
                            files_indexed: 0,
                            revisions_created: 0,
                            chunks_written: 0,
                            errors: Vec::new(),
                            duration_ms: 0,
                        },
                        removed: 0,
                        retry: paths,
                    }
                }
                Err(error) => return Err(error),
            };
            for path in outcome.retry {
                let failures = failures_of(retries, &path);
                schedule(retries, path, failures, emit);
            }
            // After, not before: the hint must never be written ahead of the
            // data it describes, and a rename's create half is that
            // data.
            record_moves(root, vault_root, events)
        }
    }
}

/// The rename hint: `source_moved(old_key, new_key)`.
///
/// A hint, and only a hint — nothing here UPDATEs a stored `source_key` (the
/// column is identity and identity is not mutable). It gets a batch of its own
/// because a rename *is* one: the `batch_id` column says "one `index`, one
/// feed poll, one rename".
///
/// Written only for a rename the debouncer actually paired. A path that
/// vanished without a partner is a deletion, and the batch's removal pass has
/// already said so.
fn record_moves(root: &Path, vault_root: &Path, events: &[DebouncedEvent]) -> Result<(), RunError> {
    let paired: Vec<&DebouncedEvent> = events
        .iter()
        .filter(|debounced| {
            matches!(
                debounced.event.kind,
                EventKind::Modify(ModifyKind::Name(RenameMode::Both))
            ) && debounced.event.paths.len() == 2
        })
        .collect();
    if paired.is_empty() {
        return Ok(());
    }
    let mut store = Store::open(database_path(root))?;
    // The shared stamp id (see `stamp_id`): a shared second and a per-call
    // index collided on `UNIQUE (batch_id, seq)` and took the watcher
    // down with it.
    let base = crate::stamp_id("move");
    for (index, debounced) in paired.iter().enumerate() {
        let from = &debounced.event.paths[0];
        let to = &debounced.event.paths[1];
        // Either side outside the vault means this is not our rename to report;
        // a half-spoken hint is worse than none.
        let (Ok(old_key), Ok(new_key)) = (
            source_key_for(vault_root, from),
            source_key_for(vault_root, to),
        ) else {
            continue;
        };
        // A directory rename is not a source move: neither side is a Source
        // identity, and the hint would name two keys that never
        // existed while the files that actually moved get none. The
        // destination is the side that exists after the rename, so asking
        // whether it is a directory is what separates the two cases.
        if to.is_dir() {
            continue;
        }
        store.append_store_event(&NewStoreEvent {
            occurred_at: momotaro_store::now_epoch_secs(),
            batch_id: &format!("{base}-{index}"),
            seq: 1,
            domain: "ingest",
            event_type: "source_moved",
            source_key: Some(&new_key),
            outcome: None,
            reason_code: None,
            message: None,
            data_json: &serde_json::json!({ "old_key": old_key, "new_key": new_key }).to_string(),
            external_id: None,
        })?;
    }
    Ok(())
}

/// Where the reconcile cookie may be written.
///
/// A hostile vault can put a link — or a hard link — at a fixed name inside the
/// vault. Writing through either would truncate a file outside it and then
/// unlink the entry to hide it, and a dangling link would *create* a file at a
/// chosen path. `is_file` alone does not help: a hard link is an ordinary file.
/// What does help is asking whether the bytes already there are ours: our own
/// leftover sentinel is, and nothing else in a vault is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CookieSlot {
    /// Nothing is there; we are about to create it.
    Absent,
    /// It already holds our sentinel — a leftover from a run that did not get
    /// to clean up. Safe to overwrite.
    Ours,
    /// Something else owns this name. Never write.
    Foreign,
}

/// Classifies the sentinel's path without writing a byte.
fn cookie_slot(path: &Path) -> CookieSlot {
    match std::fs::symlink_metadata(path) {
        Err(_) => CookieSlot::Absent,
        // A symlink (dangling or not) and a directory are never ours: reading
        // through the first could follow it anywhere, and writing through it is
        // the whole attack.
        Ok(meta) if !meta.file_type().is_file() => CookieSlot::Foreign,
        Ok(_) => match std::fs::read(path) {
            Ok(bytes) if bytes == SENTINEL_NAME.as_bytes() => CookieSlot::Ours,
            // A hard link, or any file we did not write.
            _ => CookieSlot::Foreign,
        },
    }
}

/// Writes the cookie through a handle that cannot have swapped behind the
/// classification.
///
/// On Windows, `FILE_FLAG_OPEN_REPARSE_POINT` opens the reparse point itself,
/// so a symlink/junction planted at the fixed name cannot redirect the write.
/// The handle metadata and bytes are then checked against the same open
/// object, closing the classify/open TOCTOU window.
#[cfg(windows)]
fn write_cookie(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .and_then(|mut file| {
            if !file.metadata()?.file_type().is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cookie slot is not a regular file",
                ));
            }
            let mut existing = Vec::new();
            file.read_to_end(&mut existing)?;
            if existing.is_empty() || existing == SENTINEL_NAME.as_bytes() {
                file.seek(SeekFrom::Start(0))?;
                file.set_len(0)?;
                file.write_all(SENTINEL_NAME.as_bytes())?;
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cookie slot is foreign",
                ))
            }
        })
        .is_ok()
}

/// Unix uses the same refuse-foreign rule; symlink exclusion stays in
/// `cookie_slot` via `symlink_metadata`.
#[cfg(not(windows))]
fn write_cookie(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom, Write};
    if matches!(cookie_slot(path), CookieSlot::Foreign) {
        return false;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .and_then(|mut file| {
            let mut existing = Vec::new();
            file.read_to_end(&mut existing)?;
            if existing.is_empty() || existing == SENTINEL_NAME.as_bytes() {
                file.seek(SeekFrom::Start(0))?;
                file.set_len(0)?;
                file.write_all(SENTINEL_NAME.as_bytes())?;
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "cookie slot is foreign",
                ))
            }
        })
        .is_ok()
}

/// One gated batch over the paths an event batch named.
///
/// Returns the batch rather than scheduling its own retries: the retry queue
/// owns the attempt count, and a caller that took a path out of that queue
/// must hand the count back rather than read the hole it just made.
fn rescan(
    root: &Path,
    paths: Vec<PathBuf>,
    emit: &mut dyn FnMut(&WatchLine),
) -> Result<BatchOutcome, RunError> {
    let named = paths.len() as u64;
    let only: HashSet<PathBuf> = paths.into_iter().collect();
    let outcome = run_batch(
        root,
        None,
        attributes_of,
        BatchShape::watched(WATCH_PLAN, only),
    )?;
    let removed = outcome.removed;
    let report = outcome.report;
    // A batch the gate opened for nothing has no rows to name: printing its id
    // would point a reader at a `batch_id` that `store_events` does not hold.
    if report.files_scanned == 0 && removed == 0 {
        return Ok(BatchOutcome {
            report,
            removed,
            retry: outcome.retry,
        });
    }
    emit(&WatchLine::Batch {
        batch_id: report.batch_id.clone(),
        rescanned: report.files_scanned,
        rebuilt: report.revisions_created,
        refreshed: report
            .files_indexed
            .saturating_sub(report.revisions_created),
        unchanged: report.files_skipped,
        removed,
        dropped: named.saturating_sub(report.files_scanned),
    });
    Ok(BatchOutcome {
        report,
        removed,
        retry: outcome.retry,
    })
}

/// Puts a path back on the schedule.
///
/// `failures` is what has *already* happened. The queue is the other source
/// of that number, and it is the wrong one to ask after a path has been taken
/// out of it — reading zero there restarted the escalation on every attempt,
/// which is how the delay queue and the give-up became unreachable.
fn schedule(
    retries: &mut Vec<Retry>,
    path: PathBuf,
    failures: u32,
    emit: &mut dyn FnMut(&WatchLine),
) {
    let Some(delay) = retry_delay(failures) else {
        // Done for good: leaving the entry in would make the next drain try it
        // again, for ever, with a count that can no longer grow.
        retries.retain(|retry| retry.path != path);
        emit(&WatchLine::RetriesExhausted {
            path: crate::sanitize(&path.display().to_string()),
        });
        return;
    };
    retries.retain(|retry| retry.path != path);
    retries.push(Retry {
        path,
        failures: failures + 1,
        when: Instant::now() + delay,
    });
}

/// Whether a batch-level failure is store/fs state the next backoff can cure.
///
/// SQLite busy/locked and retryable filesystem waits are transient; schema,
/// index and config failures are not, and still stop the watcher.
fn transient_watch_error(error: &RunError) -> bool {
    match error {
        RunError::Io { source } => matches!(
            source.kind(),
            std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
        ),
        RunError::Store(error) => error.is_transient(),
        _ => false,
    }
}

/// How many times `path` has already failed, before it leaves the queue.
fn failures_of(retries: &[Retry], path: &Path) -> u32 {
    retries
        .iter()
        .find(|retry| retry.path == path)
        .map_or(0, |retry| retry.failures)
}

/// Runs whatever is due on the torn queue.
fn drain_retries(
    root: &Path,
    retries: &mut Vec<Retry>,
    emit: &mut dyn FnMut(&WatchLine),
) -> Result<(), RunError> {
    let due: Vec<(PathBuf, u32)> = retries
        .iter()
        .filter(|retry| retry.when <= Instant::now())
        .map(|retry| (retry.path.clone(), retry.failures))
        .collect();
    for (path, failures) in due {
        // A path that is gone has nothing left to read — the removal pass and
        // the daily reconcile have already said so. Chasing it would reschedule
        // for ever, because a missing file never becomes stable.
        if !path.exists() {
            retries.retain(|retry| retry.path != path);
            continue;
        }
        // The stable window comes before the read, not after: a file that has
        // not stopped moving is not worth a second torn attempt.
        if mtime_stable(&path).unwrap_or(false) {
            // The entry stays in the queue while this runs, so anything that
            // asks the count reads what the path arrived with rather than the
            // hole that taking it out first would have left.
            let outcome = rescan(root, vec![path.clone()], emit)?;
            if outcome.retry.iter().any(|retry| retry == &path) {
                schedule(retries, path, failures, emit);
            } else {
                retries.retain(|retry| retry.path != path);
            }
        } else {
            schedule(retries, path, failures, emit);
        }
    }
    Ok(())
}

/// A reconciliation, with the cookie when a watcher is listening.
///
/// The cookie is written into the vault, the channel is drained until it comes
/// back, and only then does the walk start. Everything that happened before the
/// cookie has therefore been applied; everything after it arrives as an
/// ordinary event. If the cookie never comes back the walk runs anyway —
/// waiting is an optimisation, and a non-destructive command must not fail
/// because a listener was slow.
fn run_reconcile(
    root: &Path,
    vault_root: &Path,
    sentinel: &Path,
    rx: &mpsc::Receiver<DebounceEventResult>,
    trigger: Trigger,
    emit: &mut dyn FnMut(&WatchLine),
    retries: &mut Vec<Retry>,
) -> Result<(), RunError> {
    // The cookie is only ever established on a name this workspace already
    // owns; see `cookie_slot` for what a vault could put there instead.
    let slot = cookie_slot(sentinel);
    let plain = slot != CookieSlot::Foreign;
    let wrote = plain && write_cookie(sentinel);
    let deadline = Instant::now() + COOKIE_TIMEOUT;
    // Without a cookie there is nothing to wait for — but there is still
    // something to report, and `caught_up` must stay false so it is reported.
    let mut caught_up = false;
    while wrote && !caught_up && Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(events)) => {
                for event in &events {
                    if event.event.paths.iter().any(|path| path == sentinel) {
                        caught_up = true;
                    }
                }
                // Ordinary work keeps flowing while we wait; a nested request
                // for a reconciliation is dropped, because one is about to run.
                if let WatchAction::Rescan(paths) = actions_for(&events, vault_root) {
                    let outcome = rescan(root, paths, emit)?;
                    for path in outcome.retry {
                        let failures = failures_of(retries, &path);
                        schedule(retries, path, failures, emit);
                    }
                    // Work arriving while we wait is ordinary work: it gets
                    // the same hint a batch outside the window would.
                    record_moves(root, vault_root, &events)?;
                }
            }
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if wrote {
        let _ = std::fs::remove_file(sentinel);
    }
    if !caught_up {
        // The walk is still correct without the cookie — waiting is an
        // optimisation — but the reader deserves to know it ran one event
        // behind.
        let reason = if !plain {
            "the sentinel path already holds content this workspace did not write"
        } else if !wrote {
            "the sentinel could not be written"
        } else {
            "the cookie was not observed in time"
        };
        emit(&WatchLine::CookieSkipped {
            reason: reason.to_owned(),
        });
    }

    let report = reconcile(root)?;
    emit(&WatchLine::Reconciled {
        trigger,
        batch_id: report.batch_id,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, Event, EventKind, ModifyKind, RenameMode};
    use notify_debouncer_full::DebouncedEvent;

    fn vault() -> PathBuf {
        PathBuf::from("/vault")
    }

    fn debounced(event: Event) -> DebouncedEvent {
        DebouncedEvent::new(event, Instant::now())
    }

    /// The two things `cookie_slot` refuses to write through.
    ///
    /// Its comment names the reason — "reading through the first could follow
    /// it anywhere, and writing through it is the whole attack" — but neither
    /// classification had a fixture, so `Foreign` was unreachable and
    /// `run_reconcile` would have written the sentinel through whatever the
    /// path turned out to be. A directory is the half a Windows runner can
    /// always build; the symlink half needs privilege there and is exercised
    /// on the unix leg.
    #[test]
    fn a_name_that_is_not_a_regular_file_is_refused_by_the_sentinel_slot() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a_directory = dir.path().join("a-directory");
        std::fs::create_dir(&a_directory).expect("make the directory");
        assert_eq!(
            cookie_slot(&a_directory),
            CookieSlot::Foreign,
            "a directory is never ours: writing through it is the whole attack"
        );

        let missing = dir.path().join("nothing-here");
        assert_eq!(
            cookie_slot(&missing),
            CookieSlot::Absent,
            "…and a name nobody holds is the one we are about to create"
        );
    }

    /// The two classifications the type guard is actually load-bearing for.
    ///
    /// A directory is *not* one of them: without the guard it falls through
    /// to `read`, which fails, and the catch-all arm says `Foreign` anyway.
    /// What the guard exists for is a path that reads successfully but is not
    /// a file we own — and the slot's own verdict on such a path is what
    /// decides whether `run_reconcile` writes the sentinel through it.
    #[test]
    fn the_sentinel_slot_tells_ours_from_foreign_by_what_is_actually_there() {
        let dir = tempfile::tempdir().expect("temp dir");

        let ours = dir.path().join("ours");
        std::fs::write(&ours, SENTINEL_NAME).expect("write our own sentinel");
        assert_eq!(
            cookie_slot(&ours),
            CookieSlot::Ours,
            "our own bytes at our own name — safe to overwrite"
        );

        let theirs = dir.path().join("theirs");
        std::fs::write(&theirs, "something else entirely").expect("write");
        assert_eq!(
            cookie_slot(&theirs),
            CookieSlot::Foreign,
            "somebody else owns this name"
        );

        let missing = dir.path().join("nothing-here");
        assert_eq!(
            cookie_slot(&missing),
            CookieSlot::Absent,
            "…and a name nobody holds is the one we are about to create"
        );
    }

    /// The half `read` cannot answer for itself: a reparse point is not a
    /// file, but reading — and writing — through it lands somewhere else
    /// entirely. Without the type guard this junction reads our own sentinel
    /// back out of its target and the slot says `Ours`.
    ///
    /// Windows only: a directory junction needs no privilege there, while a
    /// file symlink does, and the unix leg covers the same guard with a real
    /// symlink.
    #[cfg(windows)]
    #[test]
    fn a_junction_is_foreign_whatever_its_target_holds() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("make the target");
        std::fs::write(target.join(SENTINEL_NAME), SENTINEL_NAME).expect("write the far end");

        let link = dir.path().join("link");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .expect("run mklink");
        assert!(
            status.success(),
            "a directory junction needs no privilege here"
        );

        assert_eq!(
            cookie_slot(&link),
            CookieSlot::Foreign,
            "a junction is never ours: writing through it is the whole attack"
        );
    }

    /// `DAILY` is the interval the reconcile scheduler is promised
    /// (at most once a day). Every arithmetic mutant of
    /// `24 * 60 * 60` — `+`, `/`, or either operand changed — produced a
    /// different interval and nothing in the workspace noticed: a `0` here
    /// would reconcile the whole vault on every tick of the main loop.
    #[test]
    fn the_reconcile_interval_is_a_day() {
        assert_eq!(
            DAILY.as_secs(),
            86_400,
            "the daily reconcile interval is one day"
        );
    }

    /// A rename event that does not carry a pair is skipped, not indexed.
    ///
    /// `record_moves` guards with `matches!(…RenameMode::Both) && paths.len()
    /// == 2`; relaxing the conjunction to `||` lets a one-path event into the
    /// loop that then reads `paths[1]` — an out-of-bounds panic in a watcher.
    #[test]
    fn a_rename_that_carries_one_path_is_skipped_rather_than_indexed() {
        let dir = tempfile::tempdir().expect("vault");
        let root = dir.path().to_path_buf();
        let one_path = debounced(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(root.join("only-one")),
        );
        record_moves(&root, &root, std::slice::from_ref(&one_path))
            .expect("a mis-shaped rename is skipped, not an error");
    }

    /// `same_stat` is the rule `mtime_stable` applies to the two stats it
    /// takes — and its own doc claims it is "pinned by tests that
    /// need neither the sleep nor a racing writer". Until now nothing called
    /// it: only `same_state` was pinned, so `same_stat -> true` — which would
    /// let the retry path read a file that is still moving — left the
    /// workspace green.
    ///
    /// The second stat is taken after a **deterministic** change (a longer
    /// body, so the size moves), which is what makes the claim testable
    /// without a sleep and without a racing writer: no clock granularity can
    /// hide a size difference.
    #[test]
    fn a_rewrite_that_changes_the_size_is_not_the_same_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("moving.md");
        std::fs::write(&path, "one").expect("write");

        let first = std::fs::metadata(&path).expect("first stat");
        std::fs::write(&path, "a considerably longer body").expect("rewrite");
        let second = std::fs::metadata(&path).expect("second stat");

        assert!(
            !same_stat(&first, &second),
            "a rewrite that changes the size is a different state — this is              the rule the gate applies before a retry reads again"
        );
        assert!(
            mtime_stable(&dir.path().join("missing")).is_err(),
            "…and a path nobody holds is an error, not a verdict"
        );
    }

    /// The anomaly CI cannot produce: notify's own rescan notice.
    ///
    /// `IN_Q_OVERFLOW` and FSEvents `KernelDropped` both arrive as this flag,
    /// so one synthetic value covers both platforms' real failures.
    fn dropped_events() -> Vec<DebouncedEvent> {
        vec![debounced(
            Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan),
        )]
    }

    #[test]
    fn the_rescan_flag_from_any_platform_means_reconcile() {
        let vault = vault();
        assert_eq!(
            actions_for(&dropped_events(), &vault),
            WatchAction::Reconcile(Trigger::DroppedEvents),
            "a platform that says it lost events is answered with the truth"
        );
    }

    #[test]
    fn the_blacklist_drops_what_the_walker_never_keeps() {
        let vault = vault();
        // Dot-prefixed directories and files, both shared with the
        // walker — otherwise one vault answers two ways.
        assert!(blacklisted(&vault, &vault.join(".obsidian/app.json")));
        assert!(blacklisted(&vault, &vault.join("deep/.git/config")));
        assert!(blacklisted(&vault, &vault.join(".gitignore")));
        // The five editor-temporary shapes.
        assert!(blacklisted(&vault, &vault.join("~$report.docx")));
        assert!(blacklisted(&vault, &vault.join("note.md.swp")));
        assert!(blacklisted(&vault, &vault.join("note.md~")));
        assert!(blacklisted(&vault, &vault.join("note.md.tmp")));
        assert!(blacklisted(&vault, &vault.join(".~lock.note.md#")));
        // …and the conjunct keeps both halves: `.~lock.` alone is not the rule.
        // A name that merely ends in `#` is somebody's file — `draft#`,
        // `notes.md#` — and dropping the prefix from the test would have them
        // skipped for ever, never indexed.
        assert!(!blacklisted(&vault, &vault.join("draft#")));
        assert!(!blacklisted(&vault, &vault.join("notes.md#")));
        // Anything outside the vault is not ours to scan at all.
        assert!(blacklisted(&vault, Path::new("/elsewhere/note.md")));
        // Real content: not dropped.
        assert!(!blacklisted(&vault, &vault.join("note.md")));
        assert!(!blacklisted(&vault, &vault.join("sub/note.md")));
        assert!(!blacklisted(&vault, &vault.join("a~b.md")));
        // The `leaf` conjunct on the stub rule: a hidden *directory* named
        // like an iCloud stub is still a hidden directory, and its subtree
        // drops like any dot-directory's — the walker prunes it there, and
        // keeping it here would be the one-store-two-answers shape the blacklist
        // forbids. (`.x.icloud` as a whole path IS a stub shape, but never a
        // directory in the middle of one.)
        assert!(blacklisted(&vault, &vault.join(".x.icloud/sub.md")));
    }

    #[test]
    fn the_icloud_stub_is_not_dropped() {
        // The walker keeps eviction stubs so a row can say what happened to the
        // note; a watcher that dropped them would answer differently. The
        // name has to be a *real* stub — dot-prefixed, ending `.icloud` — or
        // the assertion passes for the wrong reason: `note.md.icloud` has no
        // leading dot, so it was never hidden in the first place.
        let vault = vault();
        let stub = vault.join(".note.md.icloud");
        assert!(
            momotaro_ingest::icloud_stub(".note.md.icloud"),
            "the fixture is the iCloud eviction-stub shape"
        );
        assert!(
            !blacklisted(&vault, &stub),
            "the stub the walker keeps must reach the rescan"
        );
        assert!(
            blacklisted(&vault, &vault.join(".not-a-stub.md")),
            "a dot file that is not a stub still drops"
        );
    }

    #[test]
    fn a_rename_arrives_as_both_of_its_paths() {
        // Rename is delete+create, already folded by the debouncer
        // into one event carrying (from, to).
        let vault = vault();
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(vault.join("old.md"))
            .add_path(vault.join("new.md"));
        assert_eq!(
            actions_for(&[debounced(event)], &vault),
            WatchAction::Rescan(vec![vault.join("old.md"), vault.join("new.md")])
        );
    }

    #[test]
    fn the_sentinel_is_dropped_by_the_rule_every_other_dot_file_is() {
        // The cookie is dot-prefixed, so the blacklist is what keeps it out
        // of a rescan — one rule, not a special case beside it. The cookie
        // itself is found by scanning the raw events where it is used, which
        // is the only place that needs to see it.
        let vault = vault();
        let sentinel = vault.join(SENTINEL_NAME);
        assert!(blacklisted(&vault, &sentinel));
        let event = Event::new(EventKind::Create(CreateKind::File)).add_path(sentinel);
        assert_eq!(
            actions_for(&[debounced(event)], &vault),
            WatchAction::Nothing
        );
    }

    #[test]
    fn a_network_volume_is_refused_rather_than_degraded() {
        // Never talk a network volume into a backend this project
        // does not promise. Both calls watch a path that cannot be watched, so
        // the *reason* and the *broadcast* are what differ — and that is
        // exactly what the mutant that drops the `network` branch erases.
        let missing = std::env::temp_dir().join("momotaro-no-such-vault");

        let mut lines = Vec::new();
        let refused = open_backend(&missing, true, &mut |line| lines.push(line.clone()));
        let Err(error) = refused else {
            panic!("a path that cannot be watched must fail")
        };
        assert!(
            format!("{error}").contains("network volume"),
            "the refusal names the volume: {error}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| matches!(line, WatchLine::Degraded { .. })),
            "a network volume is never degraded into polling"
        );

        let mut lines = Vec::new();
        let local = open_backend(&missing, false, &mut |line| lines.push(line.clone()));
        // A polling backend may well accept a path the native one refused, so
        // this half asserts what it *must not* say and what it *must* say,
        // rather than requiring a particular failure.
        if let Err(error) = &local {
            assert!(
                !format!("{error}").contains("network volume"),
                "a local volume is not blamed for a missing path: {error}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, WatchLine::Degraded { .. })),
            "the fallback says so"
        );
    }

    #[test]
    fn a_paired_rename_leaves_a_source_moved_hint() {
        // The hint is written only for a rename the debouncer
        // actually paired, it never rewrites `source_key`, and it names both
        // ends. An unpaired disappearance is a deletion, and the batch's
        // removal row has already said so — so it earns no hint.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).expect("vault");
        let root = dir.path();
        // `Store::open` does not create the data directory — the workspace
        // does, and in production the startup reconciliation has always run
        // first. The test stands that up by hand for the same reason.
        std::fs::create_dir_all(root.join(".momotaro")).expect("data dir");
        let mut store = Store::open(crate::database_path(root)).expect("open");
        store.init_schema().expect("schema");
        drop(store);

        let paired = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![vault.join("old.md"), vault.join("new.md")],
            attrs: Default::default(),
        };
        let unpaired = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(vault.join("gone.md"));

        record_moves(root, &vault, &[debounced(unpaired)]).expect("no hint");
        assert_eq!(
            Store::open(crate::database_path(root))
                .expect("open")
                .latest_store_events(10)
                .expect("read")
                .len(),
            0,
            "a path that vanished with no partner is a deletion, not a move"
        );

        record_moves(root, &vault, &[debounced(paired)]).expect("hint");
        let rows = Store::open(crate::database_path(root))
            .expect("open")
            .latest_store_events(10)
            .expect("read");
        assert_eq!(rows.len(), 1, "one rename, one hint");
        assert_eq!(rows[0].event_type, "source_moved");
        assert_eq!(
            rows[0].source_key.as_deref(),
            Some("note:new.md"),
            "the hint is about where the file went"
        );
        assert!(
            rows[0].data_json.contains("note:old.md"),
            "{}",
            rows[0].data_json
        );
        assert!(
            rows[0].data_json.contains("note:new.md"),
            "{}",
            rows[0].data_json
        );

        // A directory rename writes no hint at all: it used to
        // land as `source_moved(note:sub -> note:sub2)` — two keys that never
        // existed as sources — while the files that actually moved got none.
        std::fs::create_dir_all(vault.join("sub")).expect("dir");
        std::fs::write(vault.join("sub/c.md"), "# C\n\nbody\n").expect("write");
        std::fs::rename(vault.join("sub"), vault.join("sub2")).expect("rename dir");
        let dir_rename = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![vault.join("sub"), vault.join("sub2")],
            attrs: Default::default(),
        };
        record_moves(root, &vault, &[debounced(dir_rename)]).expect("no dir hint");
        let rows = Store::open(crate::database_path(root))
            .expect("open")
            .latest_store_events(10)
            .expect("read");
        assert_eq!(
            rows.len(),
            1,
            "a directory rename writes no source_moved hint"
        );
    }

    /// Builds a workspace under `dir` that `run_reconcile` can actually walk.
    fn workspace_under(dir: &Path) -> PathBuf {
        let vault = dir.join("vault");
        std::fs::create_dir_all(&vault).expect("vault");
        std::fs::write(dir.join("momotaro.toml"), "[vault]\npath = \"vault\"\n").expect("config");
        std::fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        crate::init_workspace(dir).expect("init");
        vault
    }

    #[test]
    fn two_renames_in_the_same_second_get_two_batches() {
        // The hint used `{epoch_secs}-{index}` with the index restarting per
        // call, so two paired renames inside one second collided on
        // `UNIQUE (batch_id, seq)` and took the watcher down. The stamp has to
        // be as fine as `Batch::new`'s, and this runs both in the same second.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = workspace_under(dir.path());
        let root = dir.path();
        let mut store = Store::open(crate::database_path(root)).expect("open");
        store.init_schema().expect("schema");
        drop(store);

        let paired = |from: &str, to: &str| Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![vault.join(from), vault.join(to)],
            attrs: Default::default(),
        };

        record_moves(root, &vault, &[debounced(paired("old1.md", "new1.md"))]).expect("first hint");
        record_moves(root, &vault, &[debounced(paired("old2.md", "new2.md"))])
            .expect("second hint in the same second");

        let rows = Store::open(crate::database_path(root))
            .expect("open")
            .latest_store_events(10)
            .expect("read");
        assert_eq!(rows.len(), 2, "one hint per rename");
        assert_ne!(
            rows[0].batch_id, rows[1].batch_id,
            "each rename is its own batch, even inside one second"
        );
    }

    #[test]
    fn the_retry_schedule_escalates_and_then_gives_up() {
        // The schedule spelled out: five backoffs, the delay queue, then the daily
        // reconciliation owns it. The give-up must also take the path *out* of
        // the queue — leaving it in would make the next drain try again for
        // ever with a count that can no longer grow.
        let mut retries: Vec<Retry> = Vec::new();
        let path = PathBuf::from("/vault/a.md");
        let mut lines: Vec<WatchLine> = Vec::new();
        for failures in 0..=6u32 {
            let mut emit = |line: &WatchLine| lines.push(line.clone());
            schedule(&mut retries, path.clone(), failures, &mut emit);
            match failures {
                0..=4 => {
                    assert_eq!(retries.len(), 1, "still queued after {failures}");
                    assert_eq!(
                        retries[0].failures,
                        failures + 1,
                        "the count comes from the one handed in, not from the queue"
                    );
                }
                5 => assert_eq!(retries[0].failures, 6, "the delay queue parks it"),
                _ => assert!(retries.is_empty(), "past the delay queue it is done"),
            }
        }
        assert_eq!(lines.len(), 1, "exactly one give-up line");
        assert!(
            matches!(lines[0], WatchLine::RetriesExhausted { .. }),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn a_path_that_is_gone_leaves_the_queue() {
        // A queued file that was deleted would never become "stable", so
        // retrying it for ever is the one outcome the drain must not produce.
        let missing = std::env::temp_dir().join("momotaro-gone-for-good.md");
        let _ = std::fs::remove_file(&missing);
        let mut retries = vec![Retry {
            path: missing.clone(),
            failures: 3,
            when: Instant::now(),
        }];
        let mut lines: Vec<WatchLine> = Vec::new();
        drain_retries(Path::new("."), &mut retries, &mut |line| {
            lines.push(line.clone())
        })
        .expect("drain");
        assert!(retries.is_empty(), "it is not retried for ever");
        assert!(
            lines.is_empty(),
            "and nothing is announced: the removal pass owns its absence"
        );
    }

    #[test]
    fn a_sentinel_holding_someone_elses_bytes_is_never_written() {
        // Through the exploit a hostile vault can actually stage: a
        // hard link at the cookie's name. `is_file` says yes to that — the
        // bytes are what tell the two apart.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = workspace_under(dir.path());
        let root = dir.path();
        let victim = root.join("victim.txt");
        std::fs::write(&victim, "ORIGINAL SECRET CONTENT").expect("victim");
        let sentinel = vault.join(SENTINEL_NAME);
        std::fs::hard_link(&victim, &sentinel).expect("hard link");
        assert_eq!(cookie_slot(&sentinel), CookieSlot::Foreign, "not ours");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut lines: Vec<WatchLine> = Vec::new();
        let mut retries: Vec<Retry> = Vec::new();
        run_reconcile(
            root,
            &vault,
            &sentinel,
            &rx,
            Trigger::Startup,
            &mut |line| lines.push(line.clone()),
            &mut retries,
        )
        .expect("reconcile still runs");
        drop(tx);

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"ORIGINAL SECRET CONTENT",
            "the cookie never touched a file outside its own"
        );
        assert_eq!(
            std::fs::read(&sentinel).expect("read sentinel"),
            b"ORIGINAL SECRET CONTENT",
            "and the link still points at it"
        );
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, WatchLine::CookieSkipped { .. })),
            "the walk said it ran without the cookie"
        );
    }

    #[test]
    fn a_batch_naming_too_many_paths_is_a_storm() {
        // Both sides of the line: exactly `STORM_PATHS` is still ordinary work
        // and one more is not. Only the upper half would let a `>=` survive.
        let vault = vault();
        let paths: Vec<PathBuf> = (0..STORM_PATHS)
            .map(|index| vault.join(format!("n{index}.md")))
            .collect();
        let below = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: paths.clone(),
            attrs: Default::default(),
        };
        assert!(
            matches!(
                actions_for(&[debounced(below)], &vault),
                WatchAction::Rescan(ref found) if found.len() == STORM_PATHS
            ),
            "{STORM_PATHS} paths is still a batch of rescans"
        );

        let mut over = paths;
        over.push(vault.join("one-too-many.md"));
        let above = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: over,
            attrs: Default::default(),
        };
        assert_eq!(
            actions_for(&[debounced(above)], &vault),
            WatchAction::Reconcile(Trigger::EventStorm),
            "one more than {STORM_PATHS} is answered with the truth"
        );
    }

    /// The five optional counts are the whole difference between "what did this
    /// batch do" and a line that claims work nobody did: a zero that prints
    /// `, 0 refreshed` says the batch refreshed something, and a count that
    /// silently vanishes hides it. No test had ever built a `Batch` line —
    /// the only other `format_watch_line` test drives the `NetworkVolume` arm.
    #[test]
    fn the_batch_line_names_only_the_counts_it_has() {
        let line = |refreshed: u64, unchanged: u64, removed: u64, dropped: u64| WatchLine::Batch {
            batch_id: "b1".to_owned(),
            rescanned: 7,
            rebuilt: 2,
            refreshed,
            unchanged,
            removed,
            dropped,
        };

        let some = format_watch_line(&line(3, 2, 1, 0));
        assert!(some.contains(", 3 refreshed"), "{some}");
        assert!(some.contains(", 2 unchanged"), "{some}");
        assert!(some.contains(", 1 removed"), "{some}");
        assert!(!some.contains("dropped"), "{some}");

        let none = format_watch_line(&line(0, 0, 0, 0));
        assert!(!none.contains("refreshed"), "a zero is not a fact: {none}");
        assert!(!none.contains("unchanged"), "{none}");
        assert!(!none.contains("removed"), "{none}");
        assert!(!none.contains("dropped"), "{none}");
    }

    /// The counting contract of a real deletion batch: one deleted file is
    /// one scan, one skip and one removal — and **zero** drops. (Removals
    /// started counting into `files_scanned` with f800d71; the WatchLine-level
    /// shape had no witness, which is how the field docs drifted.)
    #[test]
    fn a_deletion_batch_counts_a_scan_a_skip_and_a_removal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = workspace_under(dir.path());
        crate::index_workspace(dir.path(), None).expect("index");
        std::fs::remove_file(vault.join("a.md")).expect("delete");

        let mut lines: Vec<WatchLine> = Vec::new();
        rescan(dir.path(), vec![vault.join("a.md")], &mut |line| {
            lines.push(line.clone())
        })
        .expect("rescan");

        let counts = lines
            .iter()
            .find_map(|line| match line {
                WatchLine::Batch {
                    rescanned,
                    rebuilt,
                    unchanged,
                    removed,
                    dropped,
                    ..
                } => Some((*rescanned, *rebuilt, *unchanged, *removed, *dropped)),
                _ => None,
            })
            .expect("a deletion is a batch");
        assert_eq!(
            counts,
            (1, 0, 1, 1, 0),
            "a deleted file is scanned, skipped and removed — never dropped"
        );
    }

    /// The guard's two halves are two different promises: no rows *and* no
    /// removals. The first was watched; the second was not, so a batch that
    /// found nothing but did notice a deletion printed an id `store_events`
    /// never held — exactly what the comment above the condition forbids.
    #[test]
    fn a_batch_that_named_nothing_prints_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = workspace_under(dir.path());
        crate::index_workspace(dir.path(), None).expect("index");

        let mut lines: Vec<WatchLine> = Vec::new();
        rescan(
            dir.path(),
            vec![vault.join("never-existed.md")],
            &mut |line| lines.push(line.clone()),
        )
        .expect("rescan");

        assert!(
            lines.is_empty(),
            "nothing happened, so nothing gets an id: {lines:?}"
        );
    }

    /// The retry queue's only read: how many times *this* path has already
    /// failed. Getting it wrong is not cosmetic — zero restarts the escalation
    /// on every attempt, which is how the delay queue and the give-up became
    /// unreachable in the first place.
    /// The cookie writer must never open a directory for writing; this is the
    /// portable half of the adversarial-slot guard (the symlink half requires
    /// platform symlink privilege and is pinned by the Windows reparse flag).
    #[test]
    fn transient_io_errors_are_retried_not_fatal() {
        let error = RunError::Io {
            source: std::io::Error::new(std::io::ErrorKind::Interrupted, "busy"),
        };
        assert!(transient_watch_error(&error));
        assert!(!transient_watch_error(&RunError::ConfigInvalid(
            "no".to_owned()
        )));
    }

    #[test]
    fn cookie_writer_refuses_a_directory_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let slot = dir.path().join(".momotaro-reconcile-sentinel");
        std::fs::create_dir(&slot).expect("plant a directory");
        assert!(!write_cookie(&slot));
        assert!(
            std::fs::read_dir(&slot).expect("directory remains").count() == 0,
            "nothing was written through the hostile slot"
        );
    }

    #[test]
    fn failures_of_reads_the_queue_for_that_path_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let here = dir.path().join("a.md");
        let elsewhere = dir.path().join("b.md");
        let retries = vec![Retry {
            path: here.clone(),
            failures: 3,
            when: Instant::now(),
        }];

        assert_eq!(failures_of(&retries, &here), 3, "the count the queue holds");
        assert_eq!(
            failures_of(&retries, &elsewhere),
            0,
            "a path with no entry has failed zero times, not one"
        );
        assert_eq!(failures_of(&[], &here), 0, "an empty queue answers nothing");
    }

    #[test]
    fn events_about_junk_reach_nothing() {
        let vault = vault();
        let event = Event::new(EventKind::Create(CreateKind::File))
            .add_path(vault.join(".obsidian/workspace.json"));
        assert_eq!(
            actions_for(&[debounced(event)], &vault),
            WatchAction::Nothing
        );
    }

    #[test]
    fn the_torn_schedule_backs_off_then_parks_then_gives_up() {
        // Five attempts, the delay queue, then the daily reconciliation owns it.
        // Pinned value by value: a schedule that drifts is a
        // schedule no test noticed.
        assert_eq!(retry_delay(0), Some(Duration::from_millis(100)));
        assert_eq!(retry_delay(1), Some(Duration::from_millis(200)));
        assert_eq!(retry_delay(2), Some(Duration::from_millis(400)));
        assert_eq!(retry_delay(3), Some(Duration::from_millis(800)));
        assert_eq!(retry_delay(4), Some(Duration::from_millis(1600)));
        assert_eq!(retry_delay(5), Some(TORN_DELAY));
        assert_eq!(retry_delay(6), None, "past the delay queue it is not ours");
        assert_eq!(retry_delay(60), None);
    }

    #[test]
    fn the_stable_window_compares_both_halves_of_a_stat() {
        // The rule, without the sleep. Each half has a case only it can fail —
        // equal size with a different mtime, and equal mtime with a different
        // size — and neither can be built from real files, because a write
        // always moves both at once.
        let then = std::time::SystemTime::UNIX_EPOCH;
        let later = then + Duration::from_secs(1);
        assert!(same_state((10, Some(then)), (10, Some(then))));
        assert!(
            !same_state((10, Some(then)), (10, Some(later))),
            "equal size, different mtime: not the same file state"
        );
        assert!(
            !same_state((10, Some(then)), (11, Some(then))),
            "different size, same mtime: not the same file state either"
        );
        assert!(!same_state((10, Some(then)), (11, Some(later))));
        assert!(
            same_state((7, None), (7, None)),
            "a stat with no readable mtime still compares its size"
        );
    }

    #[test]
    fn an_untouched_file_passes_the_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("quiet.md");
        std::fs::write(&path, "one").expect("write");
        assert!(mtime_stable(&path).expect("stat"));
    }
    /// The recheck measured this shape for real: a `\ud800`-named
    /// file made the watcher exit before the fix, and WTF-16 makes an
    /// unpaired surrogate a legal Windows file name — so the witness is the
    /// real thing, not a stand-in. It drives both repaired seams at once:
    /// the gate's name half (a `?` there answers `Err`, not a batch) and the
    /// `ForceRebuild` arm's downstream promise (nothing forced = nothing
    /// processed = no row).
    #[cfg(windows)]
    #[test]
    fn a_lone_surrogate_name_is_one_row_and_the_watcher_survives() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = workspace_under(dir.path());
        crate::index_workspace(dir.path(), None).expect("index");
        let wide: Vec<u16> = "bad-"
            .encode_utf16()
            .chain(std::iter::once(0xD800u16))
            .chain(".md".encode_utf16())
            .collect();
        let name = <std::ffi::OsString as std::os::windows::ffi::OsStringExt>::from_wide(&wide);
        let bad = vault.join(name);
        std::fs::write(&bad, "# B\n\nbody\n").expect("a lone surrogate is a legal WTF-16 name");

        let mut lines: Vec<WatchLine> = Vec::new();
        let outcome = rescan(dir.path(), vec![bad], &mut |line| lines.push(line.clone()))
            .expect("one unnameable path must not take the watcher down");
        assert!(
            outcome
                .report
                .errors
                .iter()
                .any(|error| error.kind == momotaro_contracts::IndexFileErrorKind::InvalidUtf8),
            "the pass writes its row: {:?}",
            outcome.report.errors
        );
    }
}
