//! Markdown ingestion: normalize, hash, walk, and chunk vault sources.
//!
//! Turns vault Markdown files into deterministic `SourceRevision` + `Chunk`
//! sets. Pure where possible; only `walk`/file reads touch the filesystem.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use momotaro_contracts::IndexFileErrorKind;

mod chunker;
mod hash;
mod normalize;
mod path;
mod title;
mod walk;

pub use chunker::chunk_markdown;
pub use hash::{chunk_hash, chunk_id, revision_hash};
pub use normalize::normalize_markdown;
pub use path::{canonical_workspace_root, is_within, local_path_in, resolve_local_path};
pub use title::derive_title;
pub use walk::{raw_name_for, source_key_for, walk_markdown_files};

/// Failures raised while turning vault files into chunks.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Underlying filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A source file is not valid UTF-8.
    #[error("invalid utf-8 in {0}")]
    InvalidUtf8(String),
    /// A path argument is not beneath the vault root.
    #[error("path {0} is outside vault root")]
    OutsideVault(String),
    /// A path argument is not beneath the workspace root (ADR 0024).
    #[error("path {0} is outside the workspace")]
    OutsideWorkspace(String),
    /// A derived source key violated the key contract.
    #[error("invalid source key for {0}")]
    InvalidSourceKey(String),
    /// The vault root itself does not exist.
    #[error("vault root not found: {0}")]
    VaultMissing(String),
}

impl IngestError {
    /// The machine-readable kind this failure maps onto in an index report.
    pub fn kind(&self) -> IndexFileErrorKind {
        match self {
            Self::InvalidUtf8(_) => IndexFileErrorKind::InvalidUtf8,
            Self::OutsideVault(_) | Self::OutsideWorkspace(_) => IndexFileErrorKind::OutsideVault,
            Self::InvalidSourceKey(_) => IndexFileErrorKind::InvalidSourceKey,
            Self::Io(_) | Self::VaultMissing(_) => IndexFileErrorKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_maps_onto_a_typed_kind() {
        assert_eq!(
            IngestError::InvalidUtf8("x".to_owned()).kind(),
            IndexFileErrorKind::InvalidUtf8
        );
        assert_eq!(
            IngestError::OutsideVault("x".to_owned()).kind(),
            IndexFileErrorKind::OutsideVault
        );
        assert_eq!(
            IngestError::InvalidSourceKey("x".to_owned()).kind(),
            IndexFileErrorKind::InvalidSourceKey
        );
        assert_eq!(
            IngestError::VaultMissing("x".to_owned()).kind(),
            IndexFileErrorKind::Other
        );
        assert_eq!(
            IngestError::Io(std::io::Error::other("x")).kind(),
            IndexFileErrorKind::Other
        );
    }
}
