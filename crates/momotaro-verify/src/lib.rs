//! Deterministic citation verification.
//!
//! This crate answers one question — *does this citation actually point at
//! what the store says it points at?* — and answers it without a model.
//!
//! Three properties the tests pin down:
//!
//! * **Zero tokens, zero LLM.** There is no `momotaro-llm` dependency and no
//!   provider slot here: the verifier has no slot and never spends
//!   tokens. A model cannot declare
//!   its own citations valid, because nothing in this crate can ask one.
//! * **Only an exact match passes.** `source_key + revision_hash +
//!   chunk_hash` and the locator are compared verbatim against the stored
//!   chunk row, and the quote must be found inside that chunk's text. A
//!   check that only compared lengths would pass `chash-1` for `chash-9`.
//! * **Half-checked is never `passed`.** Four states are produced:
//!   `passed` / `failed` / `unsupported` / `skipped`. Anything this build
//!   cannot decide — a store that will not read, source bytes whose hash
//!   cannot be recomputed — reports `unsupported`, and a read failure is
//!   never reported as a missing fact.
//!
//! The verdict for missing source bytes is a separate `source_integrity`
//! result rather than a citation failure, so that a citation whose file was
//! archived away still verifies from the stored chunk text while the
//! integrity check reports the loss.
//!
//! This crate never writes. Persisting `verifications` rows and the
//! `answer_citations.verified` flag is `momotaro-run`'s step 12.
//!
//! # Examples
//!
//! Judging an answer needs a store to read:
//!
//! ```no_run
//! use momotaro_store::Store;
//! use momotaro_verify::{Citation, StoreSource, VerifyPolicy, verify_answer};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = Store::open("ws/.momotaro/momotaro.db")?;
//! let source = StoreSource::new(&store, "ws");
//! let citation = Citation {
//!     context_item_id: "f0001".to_owned(),
//!     source_key: "note:notes/fourier.md".to_owned(),
//!     revision_hash: "rev-1".to_owned(),
//!     chunk_id: "chunk-1".to_owned(),
//!     chunk_hash: "chash-1".to_owned(),
//!     locator_json: r#"{"start_byte":0}"#.to_owned(),
//!     quoted_text: "energy is conserved".to_owned(),
//! };
//!
//! let verdict = verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation]);
//! println!("{:?}", verdict.citations[0].status);
//! // `fully_verified()` is the one derived flag: every citation passed and no
//! // integrity check vetoes the answer.
//! println!("fully verified: {}", verdict.fully_verified());
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::all)]
#![warn(rustdoc::unescaped_backticks)]

pub mod citation;
pub mod error;
pub mod source;
pub mod status;
pub mod verdict;
pub mod verify;

pub use citation::Citation;
pub use error::VerifyError;
pub use source::{BytesState, CitationSource, StoreSource};
pub use status::{VerificationKind, VerificationStatus, VerifyDetail};
pub use verdict::{AnswerVerdict, Verification};
pub use verify::{VerifyPolicy, verify_answer};

#[cfg(test)]
mod tests {
    /// The manifest's dependency set, written out as a literal so it cannot
    /// move with the manifest it is checked against.
    ///
    /// Written out rather than derived from the manifest: a check for one
    /// crate name would let any other client through, and "zero tokens, zero
    /// LLM" is a property of *what this crate may link against*, not of one
    /// spelling.
    const DEPENDENCIES: [&str; 8] = [
        "momotaro-contracts",
        "momotaro-store",
        "serde",
        "serde_json",
        "sha2",
        "hex",
        "tempfile",
        "thiserror",
    ];

    /// Crate names declared in one TOML dependency section, dropping the
    /// `workspace = true` suffix.
    fn declared_keys(section: &str) -> Vec<&str> {
        section
            .lines()
            .filter_map(|line| line.split('=').next())
            .map(str::trim)
            .filter(|key| !key.is_empty() && !key.starts_with('#'))
            .map(|key| key.split('.').next().unwrap_or(key))
            .collect()
    }

    /// **Zero tokens, zero LLM.** The verifier has no slot and never spends
    /// tokens — and a model cannot declare its own citations valid because
    /// nothing here can ask one. Adding a network or model client to the manifest fails this test
    /// before any prompt could be written.
    #[test]
    fn the_manifest_declares_only_the_expected_dependency_sections() {
        let manifest = include_str!("../Cargo.toml");

        // The two sections this crate is allowed to declare — and no others
        // anywhere in the file. A `[target.….dependencies]` inserted above
        // `[dependencies]` would otherwise sit outside the slice below and
        // never be parsed.
        let headers: Vec<&str> = manifest
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('[') && line.contains("dependencies]"))
            .collect();
        assert_eq!(
            headers,
            ["[dependencies]", "[dev-dependencies]"],
            "an unexpected dependency section appeared"
        );

        let dependencies = manifest
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("[dev-dependencies]").next())
            .expect("a [dependencies] section");
        let dev_dependencies = manifest
            .split("[dev-dependencies]")
            .nth(1)
            .expect("a [dev-dependencies] section");

        let mut declared = declared_keys(dependencies);
        declared.extend(declared_keys(dev_dependencies));
        declared.sort_unstable();
        let mut expected = DEPENDENCIES;
        expected.sort_unstable();
        assert_eq!(declared, expected, "the dependency set drifted");

        assert!(
            !manifest.contains("momotaro-llm"),
            "the verifier has no model slot and never spends tokens"
        );
    }
}
