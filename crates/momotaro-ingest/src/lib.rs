//! Markdown ingestion: normalize, hash, walk, and chunk vault sources.
//!
//! Turns vault Markdown files into deterministic `SourceRevision` + `Chunk`
//! sets. Pure where possible; only `walk`/file reads touch the filesystem.

#![forbid(unsafe_code)]

mod chunker;
mod hash;
mod normalize;
mod title;
mod walk;

pub use chunker::chunk_markdown;
pub use hash::{chunk_hash, chunk_id, revision_hash};
pub use normalize::normalize_markdown;
pub use title::derive_title;
pub use walk::{source_key_for, walk_markdown_files};

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
    /// The vault root itself does not exist.
    #[error("vault root not found: {0}")]
    VaultMissing(String),
}
