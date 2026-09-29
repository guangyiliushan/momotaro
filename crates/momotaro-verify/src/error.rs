//! Failures of the read seam itself.

use std::path::PathBuf;

use momotaro_store::StoreError;

/// A read the seam could not perform.
///
/// Callers never see this as a verdict: `verify_citation` and
/// `check_source_integrity` turn it into `unsupported` with a detail code
/// naming the unreadable input. A store that is down must not be reported as
/// "the cited fact does not exist" — that is the rule `light-citation` states
/// as non-negotiable, and it is the reason this type exists at all.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// A store read failed.
    #[error("store read failed: {0}")]
    Store(#[from] StoreError),

    /// Reading a source's bytes failed for a reason other than absence.
    #[error("reading {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}
