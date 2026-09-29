//! The four reconciliation triggers (D40 §4.4): one walk, four callers.
//!
//! Startup, the daily timer, `momotaro reconcile`, and a watcher that lost
//! events all end here, and all of them write the same `vault.reconcile.*`
//! batch. Reconciliation is what makes D40's first sentence true in practice —
//! events are the hint, and this is the truth they are checked against.

use std::path::Path;

use momotaro_ingest::attributes_of;
use momotaro_store::Store;
use serde::Serialize;

use crate::RunError;
use crate::database_path;
use crate::index_service::{BatchShape, RECONCILE_PLAN, run_batch};

/// What one reconciliation found (D40 §4.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    /// The `vault.reconcile.*` batch this run wrote.
    pub batch_id: String,
    /// Files the walk found.
    pub scanned: u64,
    /// Files whose bytes, path or spelling differed from what the store held.
    pub changed: u64,
    /// Keys whose absence this run recorded for the first time.
    ///
    /// Not "every key the vault no longer names": a file already recorded as
    /// gone by an earlier run is not counted again, because the state that
    /// marks it gone took it out of the set this walk consults. It is counted
    /// once more if the file comes back and then leaves a second time.
    pub removed: u64,
    /// Rows the run appended: `vault.reconcile.started`, one per file, and
    /// `vault.reconcile.finished`.
    pub events: u64,
}

/// One [`ReconcileReport`] as a sentence, for a terminal.
///
/// Shares the JSON exit's fields exactly, and `--json` prints the report
/// itself, so the two can only drift if this function does.
pub fn format_reconcile(report: &ReconcileReport) -> String {
    format!(
        "reconcile batch {}: {} scanned, {} changed, {} removed, {} events",
        report.batch_id, report.scanned, report.changed, report.removed, report.events
    )
}

/// Walk the whole vault and bring the store to it.
///
/// Every one of D40 §4.4's four triggers calls this; the trigger itself is
/// reported by the caller, which is why this type does not carry one.
pub fn reconcile(root: impl AsRef<Path>) -> Result<ReconcileReport, RunError> {
    let root = root.as_ref();
    let outcome = run_batch(root, None, attributes_of, BatchShape::full(RECONCILE_PLAN))?;
    // Read the batch back rather than tallying it while it is written: "how
    // many rows did this reconcile record" deserves the log's own answer, not
    // the writer's bookkeeping. `COUNT(*)` rather than the rows themselves —
    // materialising every row to count them is ~4 allocations each.
    let store = Store::open(database_path(root))?;
    let events = store.count_batch(&outcome.report.batch_id)?;
    Ok(ReconcileReport {
        batch_id: outcome.report.batch_id,
        scanned: outcome.report.files_scanned,
        changed: outcome.report.files_indexed,
        removed: outcome.removed,
        events,
    })
}
