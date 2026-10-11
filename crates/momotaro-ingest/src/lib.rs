//! Markdown ingestion: normalize, hash, walk, and chunk vault sources.
//!
//! Turns vault Markdown files into deterministic `SourceRevision` + `Chunk`
//! sets. Pure where possible; only `walk`/file reads touch the filesystem.
//!
//! # Examples
//!
//! Normalize a note, take its frontmatter out of the index text, and chunk the
//! body while keeping locators on the note:
//!
//! ```
//! # use momotaro_ingest::IngestError;
//! # fn main() -> Result<(), IngestError> {
//! use momotaro_contracts::IndexPolicy;
//! use momotaro_ingest::{
//!     SourceOffset, chunk_markdown_with, frontmatter, normalize_markdown, revision_hash,
//! };
//!
//! let note = normalize_markdown(
//!     b"---\ntitle: Fourier\n---\n\n# Fourier\n\nEnergy is conserved.",
//! )?;
//! let (parsed, body) = frontmatter::split(&note);
//! assert!(matches!(parsed, frontmatter::FrontmatterParse::Parsed(_)));
//! assert_eq!(body, "\n# Fourier\n\nEnergy is conserved.");
//!
//! // Locators must address the note, not the body slice: carry the offset.
//! let offset = SourceOffset::before_body(&note, body);
//! let chunks = chunk_markdown_with(
//!     "note:notes/fourier.md",
//!     &revision_hash(&note),
//!     body,
//!     &IndexPolicy::default(),
//!     offset,
//!     &[],
//!     None,
//! );
//! assert_eq!(chunks.len(), 1);
//! assert_eq!(chunks[0].text, "Energy is conserved.");
//! assert_eq!(chunks[0].heading_path.as_deref(), Some("Fourier"));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::all)]
#![warn(rustdoc::unescaped_backticks)]

use momotaro_contracts::IndexFileErrorKind;

/// The shared parsing base for this crate's three passes (`collect_blocks`,
/// `analyze`, `first_h1`): one const, so a parser option cannot land in two
/// of the three places. `union` is a `const fn` (bitflags 2.x).
pub(crate) const PARSE_OPTIONS: pulldown_cmark::Options =
    pulldown_cmark::Options::ENABLE_TABLES.union(pulldown_cmark::Options::ENABLE_WIKILINKS);

mod canvas;
mod chunker;
pub mod excalidraw;
pub mod frontmatter;
mod hash;
pub mod links;
mod normalize;
mod path;
mod portability;
mod title;
mod walk;

pub use chunker::{SourceOffset, chunk_markdown_with};
pub use excalidraw::scene_spans;
pub use hash::{chunk_hash, chunk_id, revision_hash, revision_hash_reader, sha256_of};
pub use links::{
    Extracted, Namespace, Outline, RawLink, Resolution, analyze, anchor_still_exists,
    extract_frontmatter_links, parent_dir, percent_decode, resolve,
};
pub use normalize::normalize_markdown;
pub use path::{canonical_workspace_root, is_within, local_path_in, resolve_local_path};
pub use portability::{
    attributes_of, icloud_stub, is_cloud_placeholder, windows_incompatible_components,
};
pub use title::derive_title;
pub use walk::{VaultEntry, VaultFileKind, is_hidden, raw_name_for, source_key_for, walk_entries};

/// Failures raised while turning vault files into chunks.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Underlying filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A string that must be UTF-8 is not: a name, a path component, or the
    /// file's bytes.
    #[error("invalid utf-8 in {0}")]
    InvalidUtf8(String),
    /// A path argument is not beneath the vault root.
    #[error("path {0} is outside vault root")]
    OutsideVault(String),
    /// A path argument is not beneath the workspace root.
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
            Self::Io(_) => IndexFileErrorKind::Io,
            // A missing vault root is a workspace problem, not a read failure of
            // one file; it keeps the catch-all until a kind of its own is needed.
            Self::VaultMissing(_) => IndexFileErrorKind::Other,
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
            IndexFileErrorKind::Io
        );
    }
}
