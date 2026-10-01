//! The vault watcher: events are hints, reconciliation is the truth (D40).
//!
//! Everything a platform watcher reports is treated the same way — as a hint
//! that one path may deserve a look (D40 §4.2). The hint is never the input to
//! a write: a path is gated with a stat and, when that stat is racy, with the
//! file's own bytes, and only then does anything reach the store. The four
//! reconciliation triggers (D40 §4.4) are what turns an observation the event
//! stream could not carry — a dropped event, a storm, a file changed while the
//! process was down — back into the truth.
//!
//! # What this module has no instrument for
//!
//! Two of D40's cases cannot be produced on any CI machine, and no test here
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
//!   (D40 §4.8). The verdict comes from `momotaro_platform`, whose own tests
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

use crate::index_service::{BatchOutcome, BatchShape, WATCH_PLAN, now_epoch_secs, run_batch};
use crate::reconcile::reconcile;
use crate::{NETWORK_VOLUME_NOTE, RunError, database_path, load_config, vault_scope};

/// D40 §4.5: how long a path's events are held before they are acted on.
///
/// `notify-debouncer-full` picks its own tick at a quarter of this unless told
/// otherwise, which is what keeps a burst of saves to a single pass.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// D40 §4.4: one debounce batch naming more paths than this is an event storm,
/// and is answered with a full reconciliation rather than N rescans.
///
/// A threshold, not a measurement — nothing in notify's contract describes
/// one. It sits where a single pass over the named paths would cost about what
/// a reconciliation costs, and it is a constant so a test can pin both sides.
pub const STORM_PATHS: usize = 256;

/// D40 §4.4: how long a reconcile waits for its cookie before carrying on.
///
/// Watchman errors out when its cookie does not come back; a reconcile here
/// does not. Waiting is an optimisation — walking without it is still correct,
/// just one event behind — and a non-destructive command must not fail because
/// a listener was slow.
pub const COOKIE_TIMEOUT: Duration = Duration::from_secs(5);

/// D40 §4.5: the delay queue's wait, after the five backoffs.
pub const TORN_DELAY: Duration = Duration::from_secs(30);

/// D40 §4.5: the mtime has to hold still for at least this long between two
/// stats before a torn read is retried, so a writer mid-flush cannot look
/// stable for an instant and then move again.
pub const MTIME_STABLE_GAP: Duration = Duration::from_millis(64);

/// D40 §4.4's second trigger: once a day, from inside the running watcher.
pub const DAILY: Duration = Duration::from_secs(24 * 60 * 60);

/// The sentinel file's name, in the vault root (D40 §4.4).
///
/// Dot-prefixed so the walker never indexes it and the blacklist never
/// rescans it — and recognised *before* the blacklist, which is what makes it
/// a protocol message rather than a file.
pub const SENTINEL_NAME: &str = ".momotaro-reconcile-sentinel";

/// D40 §4.6: dropped at the event layer, before any filesystem question.
///
/// Two rules, both the walker's own (§5 第 1 条): anything under a
/// dot-prefixed directory, and any dot-prefixed file — except an iCloud
/// eviction stub, which the walker keeps precisely so a row can say what
/// happened to the note it stands for. On top of those sit the five
/// editor-temporary shapes §4.6 names: a single `index` walk meets them once,
/// a live watcher would otherwise chase them on every keystroke.
///
/// The sentinel is dot-prefixed like every other leaf here, so it *is*
/// dropped — by this rule, not by an exemption. What finds it is the cookie
/// scan in `run_reconcile`, which reads the raw event stream before any name
/// rule is applied.
#[must_use]
pub fn blacklisted(vault_root: &Path, path: &Path) -> bool {
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
            // stands for (D42): dropping it here would be the two-truths case
            // §4.6 says cannot happen.
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

/// One of §4.6's five editor-temporary shapes, on a leaf name only.
///
/// `.~lock.*#` is dot-prefixed and so already covered by the rule above; it is
/// spelled out because §4.6 spells it out, and a reader checking this list
/// against the contract should find every entry.
fn editor_temporary(name: &str) -> bool {
    (name.starts_with(".~lock.") && name.ends_with('#'))
        || name.starts_with("~$")
        || name.ends_with(".swp")
        || name.ends_with('~')
        || name.ends_with(".tmp")
}

/// Which of D40 §4.4's four triggers fired.
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

/// What a debounced batch of events amounts to (D40 §4.2 / §4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchAction {
    /// Nothing survived the blacklist: the events were about junk.
    Nothing,
    /// The platform says it dropped events, or the batch was a storm. Only
    /// these two ever come from an event batch; `Startup` and `Daily` are the
    /// loop's own.
    Reconcile(Trigger),
    /// These paths deserve a look. A rename arrives as its two paths, which is
    /// D40 §4.2's delete+create already folded (ADR 0014).
    Rescan(Vec<PathBuf>),
}

/// Turns one debounced batch into what the watcher should do with it.
///
/// Pure and total: no filesystem question is asked here, because the blacklist
/// (§4.6) is a name rule, and the rescan is where facts get established.
#[must_use]
pub fn actions_for(events: &[DebouncedEvent], vault_root: &Path) -> WatchAction {
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

/// How long to wait before retry number `failures + 1` (D40 §4.5), or `None`
/// when the delay queue has been served and the path belongs to the daily
/// reconciliation.
///
/// Pure so the schedule can be pinned without sleeping: 100ms, 200ms, 400ms,
/// 800ms, 1600ms — five attempts — then the 30s delay queue, then give up.
#[must_use]
pub fn retry_delay(failures: u32) -> Option<Duration> {
    match failures {
        0..=4 => Some(Duration::from_millis(100_u64 << failures)),
        5 => Some(TORN_DELAY),
        _ => None,
    }
}

/// Whether `path` has held still for [`MTIME_STABLE_GAP`] (D40 §4.5).
///
/// Two stats that far apart and identical say the writer has stopped; anything
/// else means the file is still moving and reading it now would tear again.
pub fn mtime_stable(path: &Path) -> std::io::Result<bool> {
    let first = std::fs::metadata(path)?;
    thread::sleep(MTIME_STABLE_GAP);
    let second = std::fs::metadata(path)?;
    Ok(same_stat(&first, &second))
}

/// Whether two stats describe the same file state — the rule `mtime_stable`
/// applies to the two it takes (D40 §4.5).
///
/// Both halves are load-bearing: size alone misses a rewrite of equal length,
/// and mtime alone misses a filesystem that stamps only seconds. Pinned by
/// tests that need neither the sleep nor a racing writer.
fn same_stat(first: &std::fs::Metadata, second: &std::fs::Metadata) -> bool {
    same_state(
        (first.len(), first.modified().ok()),
        (second.len(), second.modified().ok()),
    )
}

/// Whether two stat states describe the same file (D40 §4.5's stable window).
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
        /// The `ingest.*` batch these rows belong to (ADR 0026).
        batch_id: String,
        /// Paths the gate let through.
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
        unchanged: u64,
        /// Keys the walk no longer names, which this batch recorded as gone
        /// (D40 §4.2's delete half).
        removed: u64,
        /// Paths the gate threw away and nothing else happened to *there*.
        ///
        /// A path that is gone entirely appears here **and** in `removed`:
        /// those answer different questions (the gate refused it; the vault no
        /// longer has its key), and a single deleted file is both.
        dropped: u64,
    },
    /// The recommended backend would not start and polling took over (D40 §4.1).
    Degraded {
        /// What failed, in the platform's own words.
        reason: String,
    },
    /// The vault sits on a network volume, which is not promised to be
    /// real-time (D40 §4.8, D31). The sentence rides the line itself so the
    /// JSON exit carries it too — `--json` and the human line must not
    /// promise different things.
    NetworkVolume {
        /// The shared wording: no real-time promise, and why it matters.
        message: String,
    },
    /// The reconcile cookie could not be used, so the walk ran without it
    /// (D40 §4.4): still correct, just possibly one event behind. `reason` says
    /// whether the sentinel refused to be written or simply never came back.
    CookieSkipped {
        /// Why the cookie was not established.
        reason: String,
    },
    /// A path ran out of retry attempts — a torn read that would not settle,
    /// or a file that kept moving under the commit — and was left to the daily
    /// reconciliation (D40 §4.5 / §4.7).
    RetriesExhausted {
        /// The path that gave up.
        path: String,
    },
    /// A reconciliation ran, and why (D40 §4.4).
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

/// The two backends D40 §4.1 allows, so the fallback does not make every
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
    /// never for a network volume, which D40 §4.8 refuses to pretend to support.
    Polling(notify_debouncer_full::Debouncer<PollWatcher, RecommendedCache>),
}

/// Opens the backend D40 §4.1 describes: the platform's own if it starts,
/// polling if it does not — and neither, for a network volume (D40 §4.8).
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
        // D40 §4.8: no pretending. The network-volume warning has already been
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

/// Run the watcher until the process stops (D40 §4.1 / §4.4).
///
/// The backend is opened *before* the startup reconciliation, so the event
/// stream is live when the walk begins — that is what makes D40 §4.4's cookie
/// mean anything. `emit` is called once per thing that happened and is the
/// only way anything reaches a terminal.
pub fn watch(root: impl AsRef<Path>, emit: &mut dyn FnMut(&WatchLine)) -> Result<(), RunError> {
    let root = root.as_ref();
    let config = load_config(root)?;
    let vault_root = vault_scope(root, &config)?.root;
    if !vault_root.is_dir() {
        return Err(RunError::VaultMissing { path: vault_root });
    }
    let sentinel = vault_root.join(SENTINEL_NAME);
    let network = momotaro_platform::volume_info(&vault_root).remote == Some(true);
    // D31 wants this said out loud rather than discovered later as "my edits
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
    // Trigger 1 of D40 §4.4, with the cookie.
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
            // reconciliation is what covers it (D40 §4.4).
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

/// Applies one debounced batch (D40 §4.2).
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
            let outcome = rescan(root, paths, emit)?;
            for path in outcome.retry {
                let failures = failures_of(retries, &path);
                schedule(retries, path, failures, emit);
            }
            // After, not before: the hint must never be written ahead of the
            // data it describes (ADR 0026), and a rename's create half is that
            // data.
            record_moves(root, vault_root, events)
        }
    }
}

/// ADR 0014's rename hint: `source_moved(old_key, new_key)`.
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
        store.append_store_event(&NewStoreEvent {
            occurred_at: now_epoch_secs(),
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

/// Where the reconcile cookie may be written (D40 §4.4).
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

/// One gated batch over the paths an event batch named (D40 §4.2).
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

/// Puts a path back on the schedule (D40 §4.5).
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

/// How many times `path` has already failed, before it leaves the queue.
fn failures_of(retries: &[Retry], path: &Path) -> u32 {
    retries
        .iter()
        .find(|retry| retry.path == path)
        .map_or(0, |retry| retry.failures)
}

/// Runs whatever is due on the torn queue (D40 §4.5).
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

/// D40 §4.4: a reconciliation, with the cookie when a watcher is listening.
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
    let wrote = plain && std::fs::write(sentinel, SENTINEL_NAME).is_ok();
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
        // behind (D40 §4.4).
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

    /// The anomaly CI cannot produce: notify's own rescan notice (D40 §4.4).
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
            "a platform that says it lost events is answered with the truth (D40 §4.4)"
        );
    }

    #[test]
    fn the_blacklist_drops_what_the_walker_never_keeps() {
        let vault = vault();
        // §5 第 1 条: dot-prefixed directories and files, both shared with the
        // walker — otherwise one vault answers two ways.
        assert!(blacklisted(&vault, &vault.join(".obsidian/app.json")));
        assert!(blacklisted(&vault, &vault.join("deep/.git/config")));
        assert!(blacklisted(&vault, &vault.join(".gitignore")));
        // §4.6's five editor-temporary shapes.
        assert!(blacklisted(&vault, &vault.join("~$report.docx")));
        assert!(blacklisted(&vault, &vault.join("note.md.swp")));
        assert!(blacklisted(&vault, &vault.join("note.md~")));
        assert!(blacklisted(&vault, &vault.join("note.md.tmp")));
        assert!(blacklisted(&vault, &vault.join(".~lock.note.md#")));
        // Anything outside the vault is not ours to scan at all.
        assert!(blacklisted(&vault, Path::new("/elsewhere/note.md")));
        // Real content: not dropped.
        assert!(!blacklisted(&vault, &vault.join("note.md")));
        assert!(!blacklisted(&vault, &vault.join("sub/note.md")));
        assert!(!blacklisted(&vault, &vault.join("a~b.md")));
    }

    #[test]
    fn the_icloud_stub_is_not_dropped() {
        // The walker keeps eviction stubs so a row can say what happened to the
        // note (D42); a watcher that dropped them would answer differently. The
        // name has to be a *real* stub — dot-prefixed, ending `.icloud` — or
        // the assertion passes for the wrong reason: `note.md.icloud` has no
        // leading dot, so it was never hidden in the first place.
        let vault = vault();
        let stub = vault.join(".note.md.icloud");
        assert!(
            momotaro_ingest::icloud_stub(".note.md.icloud"),
            "the fixture is the shape D42 describes"
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
        // D40 §4.2: rename is delete+create, already folded by the debouncer
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
        // The cookie is dot-prefixed, so §4.6's blacklist is what keeps it out
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
        // D40 §4.8: never talk a network volume into a backend this project
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
        // ADR 0014 + §4.2: the hint is written only for a rename the debouncer
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
        // D40 §4.5 spelled out: five backoffs, the delay queue, then the daily
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
        // D40 §4.4 through the exploit a hostile vault can actually stage: a
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
        // Five attempts, the delay queue, then the daily reconciliation owns it
        // (D40 §4.5). Pinned value by value: a schedule that drifts is a
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
}
