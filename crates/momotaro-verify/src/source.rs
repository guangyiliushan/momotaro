//! The read seam: everything this crate learns about the store goes through
//! [`CitationSource`].

use momotaro_contracts::Chunk;
use momotaro_store::Store;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::error::VerifyError;

/// Same identity domain as ingest's revision hashes.
const REVISION_DOMAIN: &[u8] = b"momotaro-revision-v1\0";

/// Marks a trait as implementable only where its author can see it.
///
/// Only this crate's [`StoreSource`] implements [`CitationSource`], which is
/// what lets the trait grow a method without breaking anybody. Tests inside
/// this crate still implement it — that is the whole point of the seam.
pub(crate) mod sealed {
    /// Sealing marker; see the module docs.
    pub trait Sealed {}
}

/// What a source's on-disk bytes look like for one revision.
///
/// `reference.md` defines source integrity as "the bytes named by the row are
/// present **and match the hash**". The production seam therefore reads the
/// canonical local bytes and streams their ingest-domain SHA-256. ADR 0023
/// excludes paper bytes from snapshots; that is about backup copies, not a
/// licence to call an unchecked file verified. A read that cannot hash keeps
/// `hash_matches: None`, which the caller reports as `unsupported`, never as
/// `passed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BytesState {
    /// The bytes are readable.
    Present {
        /// `Some(true)` / `Some(false)` when the caller could recompute the
        /// revision hash; `None` when it could not.
        hash_matches: Option<bool>,
    },
    /// The revision names bytes that are not there.
    Missing,
    /// The source has no local bytes to check at all: no store row, a row
    /// with no `local_path`, or a stored path this crate refuses — by form
    /// (`..`, rooted, drive-relative) or because it resolves outside the
    /// workspace through a link (ADR 0024). A path that exists but resolves
    /// nowhere is not this state: it is [`Self::Missing`].
    Absent,
}

/// Every store-shaped fact a citation check needs.
///
/// Injected rather than reached for directly so tests can build any five-tuple
/// state — including ones the store cannot be talked into — and so the caller
/// can decide where the data comes from. It is deliberately narrow: this crate
/// never writes.
pub trait CitationSource: sealed::Sealed {
    /// Reads one chunk by its deterministic id.
    fn chunk(&self, chunk_id: &str) -> Result<Option<Chunk>, VerifyError>;

    /// Reports the state of the bytes named by one source revision.
    fn source_bytes(
        &self,
        source_key: &str,
        revision_hash: &str,
    ) -> Result<BytesState, VerifyError>;
}

/// The production seam: facts read straight from a [`Store`].
pub struct StoreSource<'a> {
    store: &'a Store,
    workspace_root: PathBuf,
}

impl<'a> StoreSource<'a> {
    /// Binds the seam to a store and to the workspace that store describes.
    ///
    /// `workspace_root` is required because `source_revisions.local_path` is
    /// workspace-relative (ADR 0024). This crate will not guess the root from
    /// the database path — that relationship is a layout convention, not a
    /// contract. A relative root is resolved **on every read** against the
    /// process's current directory — the constructor stores the spelling and
    /// resolves nothing — the same way `momotaro_ingest::path`'s helper
    /// absolutizes its input; it does not have to be absolute.
    pub fn new(store: &'a Store, workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            store,
            workspace_root: workspace_root.into(),
        }
    }
}

impl sealed::Sealed for StoreSource<'_> {}

impl CitationSource for StoreSource<'_> {
    fn chunk(&self, chunk_id: &str) -> Result<Option<Chunk>, VerifyError> {
        Ok(self.store.get_chunk(chunk_id)?)
    }

    fn source_bytes(
        &self,
        source_key: &str,
        revision_hash: &str,
    ) -> Result<BytesState, VerifyError> {
        // `current_revision` is the only revision read the store exposes; a
        // cited revision that is no longer current has no bytes on disk,
        // because `local_path` always describes the current one.
        let Some(revision) = self.store.current_revision(source_key)? else {
            return Ok(BytesState::Absent);
        };
        let Some(local_path) = revision.local_path else {
            return Ok(BytesState::Absent);
        };
        if revision.revision_hash != revision_hash {
            return Ok(BytesState::Missing);
        }

        // Three steps, in the same order `momotaro_ingest::path::
        // resolve_local_path` takes them — that module is the other reader of
        // this column, and two readers disagreeing about containment is
        // exactly the defect this sequence exists to prevent. It is repeated
        // rather than called because `momotaro-ingest` is not a dependency of
        // this crate, and adding it would change `Cargo.lock`.
        //
        // 1. Reject by form. `is_absolute()` is not enough: a rooted path
        //    with no drive prefix (`\notes\a.md`) and a drive-relative one
        //    (`C:notes\a.md`) are both *relative* by that test on Windows yet
        //    both leave the workspace. A leading `./` is well-formed for this
        //    column — ingest accepts it — and cannot leave on its own.
        let relative = Path::new(&local_path);
        for component in relative.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Ok(BytesState::Absent);
                }
            }
        }

        // 2. Canonicalize. The form check cannot see a symlink or a junction,
        //    and `metadata` follows both. The path we *read* is built from the
        //    caller's root so the stored spelling — trailing separator
        //    included — reaches the OS unchanged; the canonical form is used
        //    only for the containment test. `a_read_failure_is_reported_as_
        //    unavailable_not_as_missing` fails if `joined` is built from the
        //    canonical root instead.
        let Some(root) = canonicalize_existing_ancestor(&self.workspace_root)? else {
            // The workspace root is itself a link we cannot follow: nothing
            // under it can be there. That is a confirmed absence, not an I/O
            // failure — reporting it `unsupported` would let an answer with no
            // reachable source come back fully verified.
            return Ok(BytesState::Missing);
        };
        let joined = self.workspace_root.join(relative);
        let Some(resolved) = canonicalize_existing_ancestor(&joined)? else {
            // Exactly the predicate the root arm answers — the entry exists
            // and resolves nowhere — so exactly the same verdict: the bytes
            // named by the row are provably absent (ADR 0024, "a missing file
            // is `source_missing`"), and `fully_verified()` vetoes on it.
            // `metadata` is never called on the path, so the link is not read
            // through either; that half is pinned by
            // `a_junction_that_walks_out_of_the_workspace_is_not_read`.
            return Ok(BytesState::Missing);
        };

        // 3. Re-check containment between two canonicalized paths (ADR 0024:
        //    "the result must stay inside the workspace, including no symlink
        //    escape").
        if !resolved.starts_with(&root) {
            return Ok(BytesState::Absent);
        }

        // Every path-shaped failure we could construct reached this arm
        // through `fs::canonicalize`, so the walk-up does not surface it: this
        // is the residue — the window between those two syscalls, where the
        // entry is replaced or unpermissioned in between. It is a defensive
        // arm with no fixture (mutating it away is a registered survivor,
        // O05). O05 covers all three sites of this shape — this one and the
        // two `ErrorKind::NotFound` guards below. Not for want of
        // reachability: every missing leaf walks `canonicalize` → NotFound →
        // `symlink_metadata` → NotFound, so both guards fire on the ordinary
        // absence path. What no fixture can produce is a `symlink_metadata`
        // that answers with some *other* error while `canonicalize` has just
        // answered NotFound — that disagreement lives in the window between
        // the two syscalls — so `== NotFound` → `true` cannot be told apart
        // from the correct guard. What is held by the tests is the
        // *judgement* these arms carry: a read that never ran is
        // `unsupported`, never `source_missing`. The *judgement* it carries — a read failure is `unsupported`,
        // never `source_missing` — is pinned through the walk-up arm by
        // `a_read_failure_is_reported_as_unavailable_not_as_missing` and at
        // the mapping by
        // `a_source_that_cannot_be_read_is_unsupported_rather_than_missing`.
        //
        // Swapping this `fs::metadata` for `fs::symlink_metadata` would report
        // a *resolvable* link as missing; the fixture that kills that
        // (`a_resolvable_symlink_leaf_is_read_through`) is `#[cfg(unix)]`,
        // because a Windows junction cannot resolve onto a file and file
        // symlinks need privilege here. So that mutation survives on Windows
        // and dies on the ubuntu leg: registered survivor Q03, a platform
        // survivor rather than an uncovered arm.
        match std::fs::metadata(&joined) {
            Ok(meta) if meta.is_file() => {
                // Metadata and open are two syscalls, so a file removed in
                // between is still a confirmed absence, not an I/O outage.
                match revision_hash_matches(&joined, revision_hash) {
                    Ok(hash_matches) => Ok(BytesState::Present {
                        hash_matches: Some(hash_matches),
                    }),
                    Err(VerifyError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Ok(BytesState::Missing)
                    }
                    Err(source) => Err(source),
                }
            }
            Ok(_) => Ok(BytesState::Missing),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BytesState::Missing),
            Err(source) => Err(VerifyError::Io {
                path: joined,
                source,
            }),
        }
    }
}

/// Streams the canonical local bytes and compares their ingest-domain hash.
///
/// A 16 KiB window matches ingest's attachment reader: a paper or video can be
/// large, but integrity is a one-pass read rather than a snapshot copy. Any
/// read error propagates with the path so the caller can keep it
/// `unsupported` rather than confuse an outage with absence.
fn revision_hash_matches(path: &Path, expected: &str) -> Result<bool, VerifyError> {
    let file = std::fs::File::open(path).map_err(|source| VerifyError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DOMAIN);

    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|source| VerifyError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()) == expected)
}

/// Resolves `path`, tolerating a missing leaf: the deepest ancestor that
/// exists is canonicalized and the rest re-appended.
///
/// The walk is the same shape as
/// `momotaro_ingest::path::canonicalize_existing_ancestor` — a citation may
/// point at a file that has since been deleted, and "the bytes named by the
/// row are gone" is a fact about the row, not about whether its parent
/// directory happens to exist. A relative input is resolved against
/// `current_dir()` **on every call**, exactly as ingest's helper does, so a
/// relative `workspace_root` resolves the same way here as it does there —
/// and, as there, a drive-relative spelling (`C:notes/a.md`) keeps its prefix
/// through that join rather than becoming absolute.
///
/// One deliberate difference — between the two *readers of this column*, not
/// between two copies of a helper: when an entry exists but does not resolve,
/// a symlink or junction whose target is gone, this returns [`Option::None`]
/// instead of a path. Ingest's walk re-appends the name and resolves; this
/// reader refuses, because re-appending would produce a path that is
/// lexically inside the workspace while `metadata` reads through the link
/// outside it, and the caller reports that as a confirmed absence rather than
/// as "could not decide". On dangling values this reader is deliberately the
/// stricter of the two.
///
/// A failure other than `NotFound` is propagated as [`VerifyError::Io`] so a
/// refused read is reported `unsupported`, never `source_missing`.
fn canonicalize_existing_ancestor(path: &Path) -> Result<Option<PathBuf>, VerifyError> {
    // Absolutizing an already-absolute path is a no-op, so a mutation that
    // drops this branch is equivalent — recorded here rather than counted as
    // covered (lens K NB2).
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // A failing `current_dir()` (a deleted cwd) is unreachable by test
        // for the same reason the `fs::metadata` error arm below is: hostile
        // to a parallel suite. Registered as a survivor rather than claimed as
        // covered. `path` carries the input being resolved because there is no
        // cwd path to name when the call itself is what failed.
        std::env::current_dir()
            .map_err(|source| VerifyError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut current: &Path = &absolute;
    loop {
        match std::fs::canonicalize(current) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(Some(resolved));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(current) {
                    // Present, yet unresolvable: a link to something that is
                    // no longer there. Refuse instead of walking past it.
                    Ok(_) => return Ok(None),
                    Err(link_error) if link_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(VerifyError::Io {
                            path: current.to_path_buf(),
                            source,
                        });
                    }
                }
                // Walking all the way out to a path with no name (a bare
                // share, an empty string) is an *unreachable* root, not an
                // absent file: report an I/O failure and let
                // `source_bytes_unavailable` veto the answer. Turning this
                // into `Ok(None)` — a review proposed it as "the better
                // program" — would report an offline server as
                // `source_missing`, which `light-citation` forbids outright:
                // a network failure is never "this work does not exist".
                // Registered survivor (N12), deliberately not adopted.
                let Some(name) = current.file_name() else {
                    return Err(VerifyError::Io {
                        path: current.to_path_buf(),
                        source: error,
                    });
                };
                missing.push(name.to_os_string());
                match current.parent() {
                    Some(parent) => current = parent,
                    None => {
                        // As far out as the walk can go with no parent left
                        // to try. Treat it as an unreadable path, not as an
                        // absence: no constructed input reaches it, so it is
                        // registered rather than claimed as covered (lens K
                        // NB1).
                        return Err(VerifyError::Io {
                            path: current.to_path_buf(),
                            source: error,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(VerifyError::Io {
                    path: current.to_path_buf(),
                    source,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{VerificationKind, VerificationStatus, VerifyDetail};
    use crate::verify::{check_source_integrity, verify_citation};
    use crate::{Citation, VerifyPolicy};
    use momotaro_contracts::{OriginClass, SourceKind};
    use momotaro_store::NewSourceRevision;

    const SOURCE_KEY: &str = "note:notes/a.md";
    const REVISION: &str = "rev-1";
    const LOCATOR: &str =
        r#"{"start_byte":0,"end_byte":5,"start_line":1,"end_line":1,"truncated":false}"#;

    /// Declared store-first so the connection closes before the directory that
    /// holds it goes away.
    struct Fixture {
        store: Store,
        workspace: tempfile::TempDir,
    }

    impl Fixture {
        fn root(&self) -> &Path {
            self.workspace.path()
        }
    }

    fn chunk() -> Chunk {
        Chunk {
            chunk_id: "cid-1".to_owned(),
            source_key: SOURCE_KEY.to_owned(),
            revision_hash: REVISION.to_owned(),
            ordinal: 0,
            heading_path: None,
            text: "hello world".to_owned(),
            locator_json: LOCATOR.to_owned(),
            chunk_hash: "chash-1".to_owned(),
            token_estimate: 2,
        }
    }

    fn citation() -> Citation {
        Citation {
            context_item_id: "f0001".to_owned(),
            source_key: SOURCE_KEY.to_owned(),
            revision_hash: REVISION.to_owned(),
            chunk_id: "cid-1".to_owned(),
            chunk_hash: "chash-1".to_owned(),
            locator_json: LOCATOR.to_owned(),
            quoted_text: "hello".to_owned(),
        }
    }

    fn revision<'a>(revision_hash: &'a str, local_path: Option<&'a str>) -> NewSourceRevision<'a> {
        NewSourceRevision {
            source_key: SOURCE_KEY,
            revision_hash,
            kind: SourceKind::Note,
            title: Some("A"),
            uri: None,
            local_path,
            raw_name: Some("a.md"),
            origin_class: OriginClass::Owner,
            metadata_json: "{}",
            ingested_at: 1,
        }
    }

    fn revision_hash(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(super::REVISION_DOMAIN);
        hasher.update(bytes);
        hex::encode(hasher.finalize())
    }

    /// Builds a store with one revision, its chunk row, and — when
    /// `local_path` is given — the bytes it names.
    fn fixture(local_path: Option<&str>) -> Fixture {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let mut store = Store::open(workspace.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("init schema");
        if let Some(relative) = local_path {
            let full = workspace.path().join(relative);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(&full, "hello world").expect("write source bytes");
        }
        store
            .append_revision(&revision(REVISION, local_path))
            .expect("append revision");
        store
            .replace_chunks(SOURCE_KEY, REVISION, std::slice::from_ref(&chunk()), &[])
            .expect("chunks");
        Fixture { store, workspace }
    }

    fn verdict(source: &StoreSource<'_>, citation: &Citation) -> crate::Verification {
        verify_citation(source, VerifyPolicy::enabled(), &["f0001"], citation)
    }

    fn integrity(
        source: &StoreSource<'_>,
        source_key: &str,
        revision_hash: &str,
    ) -> crate::Verification {
        check_source_integrity(source, VerifyPolicy::enabled(), source_key, revision_hash)
    }

    #[test]
    fn a_chunk_row_in_a_real_store_makes_the_five_tuple_pass() {
        let f = fixture(Some("notes/a.md"));
        let source = StoreSource::new(&f.store, f.root());
        let v = verdict(&source, &citation());
        assert_eq!(v.kind, VerificationKind::Citation);
        assert_eq!(
            v.status,
            VerificationStatus::Passed,
            "detail: {:?}",
            v.detail
        );
    }

    #[test]
    fn a_tampered_chunk_hash_fails_against_the_real_row() {
        let f = fixture(Some("notes/a.md"));
        let source = StoreSource::new(&f.store, f.root());
        let mut cited = citation();
        cited.chunk_hash = "chash-9".to_owned();
        assert_eq!(cited.chunk_hash.len(), chunk().chunk_hash.len());
        let v = verdict(&source, &cited);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::HashMismatch));
    }

    #[test]
    fn a_chunk_row_that_is_not_in_the_real_store_reports_missing() {
        let mut f = fixture(None);
        f.store
            .replace_chunks(SOURCE_KEY, REVISION, &[], &[])
            .expect("drop the rows");
        let source = StoreSource::new(&f.store, f.root());
        let v = verdict(&source, &citation());
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::ChunkMissing));
    }

    /// The production seam does not stop at "the path exists": it hashes the
    /// canonical bytes with ingest's domain and can therefore award an
    /// integrity pass.
    #[test]
    fn matching_local_bytes_are_read_and_hashed_to_passed() {
        let mut f = fixture(Some("notes/a.md"));
        let expected = revision_hash(b"hello world");
        f.store
            .append_revision(&NewSourceRevision {
                ingested_at: 2,
                ..revision(&expected, Some("notes/a.md"))
            })
            .expect("append the real revision");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, &expected);
        assert_eq!(v.kind, VerificationKind::SourceIntegrity);
        assert_eq!(
            v.status,
            VerificationStatus::Passed,
            "detail: {:?}",
            v.detail
        );
        assert_eq!(v.detail, None);
    }

    /// Mutation twins of the pass case: same path, same read, one changed
    /// byte must fail and veto the answer rather than remain unchecked.
    #[test]
    fn tampered_local_bytes_fail_the_streamed_revision_hash() {
        let mut f = fixture(Some("notes/a.md"));
        let expected = revision_hash(b"hello world");
        f.store
            .append_revision(&NewSourceRevision {
                ingested_at: 2,
                ..revision(&expected, Some("notes/a.md"))
            })
            .expect("append the real revision");
        std::fs::write(f.root().join("notes/a.md"), "hello wormd").expect("tamper");

        let source = StoreSource::new(&f.store, f.root());
        assert_eq!(
            source.source_bytes(SOURCE_KEY, &expected).expect("read"),
            BytesState::Present {
                hash_matches: Some(false)
            }
        );
        let v = integrity(&source, SOURCE_KEY, &expected);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::RevisionHashMismatch));

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            !answer.fully_verified(),
            "an integrity hash mismatch must veto the answer"
        );
    }

    #[test]
    fn bytes_that_are_not_on_disk_report_source_integrity_failed() {
        let f = fixture(Some("notes/a.md"));
        let source = StoreSource::new(&f.store, f.root());
        std::fs::remove_file(f.root().join("notes/a.md")).expect("delete the source");
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));
    }

    #[test]
    fn a_revision_that_is_no_longer_current_reports_missing_bytes() {
        // The bytes on disk are the current revision's; a citation bound to
        // an older one names bytes that are gone.
        let mut f = fixture(Some("notes/a.md"));
        f.store
            .append_revision(&NewSourceRevision {
                ingested_at: 2,
                revision_hash: "rev-2",
                ..revision("rev-2", Some("notes/a.md"))
            })
            .expect("second revision");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));
    }

    #[test]
    fn a_source_with_no_local_path_is_unsupported_rather_than_failed() {
        let f = fixture(None);
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    #[test]
    fn a_source_key_the_store_does_not_know_is_unsupported_rather_than_failed() {
        let f = fixture(Some("notes/a.md"));
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, "note:nope.md", REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    #[test]
    fn a_local_path_that_would_escape_the_workspace_is_not_read() {
        // `local_path` is workspace-relative by contract (ADR 0024); this
        // crate refuses anything that is not, rather than resolving it.
        let f = fixture(Some("../../outside.md"));
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// A path that is rooted but carries no drive prefix is **not** absolute
    /// on Windows, so `is_absolute()` alone waves it through — and joining it
    /// resolves against the drive root, outside the workspace. Only paths
    /// whose every component is a plain name may be opened.
    #[test]
    fn a_rooted_local_path_is_not_read_either() {
        // The premise the whole test rests on: on Windows a rooted path with
        // no drive prefix is *not* absolute, so `is_absolute()` alone waves
        // it through. If that ever stopped being true the fixture would pass
        // for an unrelated reason.
        #[cfg(windows)]
        assert!(
            !Path::new("/outside/workspace.md").is_absolute(),
            "on Windows this path is relative, which is the whole point of the fixture"
        );

        let mut f = fixture(None);
        // The premise: joining a rooted path replaces the base, so this one
        // lands *outside* the workspace and containment would refuse it too.
        // Together with `a_rooted_path_that_resolves_inside_is_still_refused`
        // — whose path resolves inside — the pair is what separates the form
        // check from the containment check. The leading separator is the
        // premise, which is what this lint is asking about.
        #[allow(clippy::join_absolute_paths)]
        let joined = f.root().join("/outside/workspace.md");
        assert!(
            !joined.starts_with(f.root()),
            "premise: joining a rooted path must replace the base and leave the workspace"
        );

        f.store
            .update_revision_fingerprint(
                SOURCE_KEY,
                REVISION,
                "{}",
                "/outside/workspace.md",
                "workspace.md",
            )
            .expect("point the revision at a rooted path");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(
            v.detail,
            Some(VerifyDetail::NoLocalBytes),
            "a rooted path must be refused, not resolved against a drive root"
        );
    }

    /// `C:outside.md` is drive-relative, not absolute: it resolves against
    /// the current directory *on that drive*. Windows-only fixture — the
    /// ubuntu CI leg does not build this test.
    #[cfg(windows)]
    #[test]
    fn a_drive_relative_local_path_is_not_read() {
        let mut f = fixture(None);
        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "C:outside.md", "outside.md")
            .expect("point the revision at a drive-relative path");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// A directory sitting at `local_path` names no readable bytes.
    #[test]
    fn a_local_path_that_names_a_directory_is_missing_not_readable() {
        let mut f = fixture(None);
        std::fs::create_dir_all(f.root().join("notes")).expect("mkdir");
        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "notes", "notes")
            .expect("point the revision at a directory");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));
    }

    /// `momotaro_ingest::path::resolve_local_path` accepts a leading `./`
    /// for this very column, and a file that is not there is `source_missing`
    /// — not "I could not decide" (ADR 0024: *a missing file is
    /// `source_missing`, not a fallback*; light-skills `CONFIRMED-MISSING` →
    /// `failed`, distinct from `UNAVAILABLE` → `unsupported`).
    #[test]
    fn a_leading_dot_local_path_that_is_missing_is_still_source_missing() {
        let mut f = fixture(None);
        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "./notes/gone.md", "gone.md")
            .expect("point the revision at a ./ path");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(
            v.status,
            VerificationStatus::Failed,
            "a confirmed absence must not be downgraded to 'cannot decide'"
        );
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));
    }

    /// A path whose every component is a plain name can still walk out of the
    /// workspace through a reparse point, and ADR 0024 requires containment
    /// *including no symlink escape*. Junctions need no elevation, so this is
    /// the escape shape that exists in the wild.
    #[cfg(windows)]
    #[test]
    fn a_junction_that_walks_out_of_the_workspace_is_not_read() {
        let mut f = fixture(Some("notes/a.md"));
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("secret.md"), "s").expect("outside bytes");
        let link = f.root().join("link");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(
            created,
            "this test needs to create a junction; a silent skip would make \
             \"did not run\" look exactly like \"passed\""
        );

        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "link/secret.md", "secret.md")
            .expect("point the revision through the junction");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(
            v.status,
            VerificationStatus::Unsupported,
            "a path resolved outside the workspace must not be read at all"
        );
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// The ubuntu CI leg's half of the same property (symlink creation is
    /// `cfg(unix)`; Windows needs elevation for symlinks but not junctions).
    #[cfg(unix)]
    #[test]
    fn a_symlink_that_walks_out_of_the_workspace_is_not_read() {
        let mut f = fixture(Some("notes/a.md"));
        let outside = tempfile::tempdir().expect("outside dir");
        std::fs::write(outside.path().join("secret.md"), "s").expect("outside bytes");
        std::os::unix::fs::symlink(outside.path(), f.root().join("link")).expect("symlink");

        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "link/secret.md", "secret.md")
            .expect("point the revision through the symlink");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// A read the OS *prevented* is `unsupported`, never `source_missing`:
    /// `light-citation`'s `UNAVAILABLE` (transport, permission, rate limit) is
    /// precisely "we could not ask", and asking is what `source_missing`
    /// requires. A trailing separator on a path that names a file is the
    /// portable shape of that here — `fs::canonicalize` refuses the path
    /// before `fs::metadata` is ever reached, so the bytes question is never
    /// asked — and it is raised by the walk-up, which is why the
    /// `fs::metadata` error arm below carries no fixture of its own.
    ///
    /// **The split, as the code actually implements it:** `source_missing`
    /// when the walk completed and `fs::metadata` — the real "are the bytes
    /// there" question — answered no; `source_bytes_unavailable` when the OS
    /// refused the path before that question could be asked. It is *not* a
    /// ranking of evidence strength: measured on both CI legs, a `local_path`
    /// that goes through a plain file (`blocker/x.md`) answers `NotFound` on
    /// Windows and `NotADirectory` on Linux, so one stored value lands on
    /// opposite sides per platform. Both that shape and a trailing separator
    /// are malformed `local_path` values — `local_path_in` never emits either
    /// — so the divergence is registered here rather than designed around.
    ///
    /// A third asymmetry, from `std`: `components()` normalizes a trailing
    /// `.` away but not a trailing `/`, so `target.md/.` resolves and
    /// `target.md/` does not — the same bytes, two verdicts. Normalizing it
    /// here would make this reader disagree with `momotaro-ingest::path`
    /// (which carries the spelling through to its own `stat`), so it is
    /// registered, not papered over: a malformed stored spelling that we
    /// cannot open is reported `unsupported`, and `fully_verified()` does not
    /// claim otherwise.
    #[test]
    fn a_read_failure_is_reported_as_unavailable_not_as_missing() {
        let workspace = tempfile::tempdir().expect("temp workspace");
        let mut store = Store::open(workspace.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("init schema");
        std::fs::write(workspace.path().join("notes"), "a file, not a directory").expect("write");
        store
            .append_revision(&revision(REVISION, Some("notes/")))
            .expect("append revision");
        store
            .replace_chunks(SOURCE_KEY, REVISION, std::slice::from_ref(&chunk()), &[])
            .expect("chunks");

        let source = StoreSource::new(&store, workspace.path());
        let v = check_source_integrity(&source, VerifyPolicy::enabled(), SOURCE_KEY, REVISION);
        assert_eq!(
            v.status,
            VerificationStatus::Unsupported,
            "detail: {:?}",
            v.detail
        );
        assert_eq!(v.detail, Some(VerifyDetail::SourceBytesUnavailable));

        // The "failed check" side of the split, at answer level: the file is
        // really there, the OS just refused the path as written — so this is
        // `unsupported`, never `source_missing`, *and* because that read never
        // ran the answer is not fully verified.
        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            !answer.fully_verified(),
            "an integrity check that never ran must not leave the answer verified"
        );
    }

    /// A missing leaf under a directory that *does* exist: the walk completes,
    /// `fs::metadata` answers no, so this is a confirmed absence —
    /// `source_missing`, which vetoes. That is the other side of the split
    /// `a_read_failure_is_reported_as_unavailable_not_as_missing` states, and
    /// unlike a `local_path` that goes through a plain file this shape
    /// answers `NotFound` on both CI legs, so the fixture is platform-stable.
    #[test]
    fn a_missing_leaf_under_a_real_directory_is_a_confirmed_absence() {
        let mut f = fixture(None);
        std::fs::create_dir_all(f.root().join("holder")).expect("mkdir");
        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "holder/gone.md", "gone.md")
            .expect("point the revision at a leaf that is not there");
        let source = StoreSource::new(&f.store, f.root());

        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(!answer.fully_verified());
    }

    /// A workspace root on a server that is not there. This is a *transport*
    /// failure, not a statement about the bytes: `light-citation`'s
    /// non-negotiable rule is that a network failure is never reported as
    /// "this work does not exist", so the verdict stays `unsupported` — but
    /// the answer still may not come back fully verified, because nothing was
    /// checked. Killing the `file_name() == None` exit into `Ok(None)` here
    /// would flip this to `source_missing` and break that rule.
    #[cfg(windows)]
    #[test]
    fn a_workspace_root_on_an_unreachable_server_is_unverified_not_absent() {
        let f = fixture(Some("notes/a.md"));
        let source = StoreSource::new(&f.store, r"\\no-such-server-momotaro\share\ws");

        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(
            v.status,
            VerificationStatus::Unsupported,
            "detail: {:?}",
            v.detail
        );
        assert_eq!(
            v.detail,
            Some(VerifyDetail::SourceBytesUnavailable),
            "a transport failure must not be reported as an absence"
        );

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            !answer.fully_verified(),
            "an answer whose workspace root could not be reached was never checked"
        );
    }

    /// A *resolvable* symlink as the leaf. Every other link fixture is either
    /// an escape (refused by containment before `metadata` runs) or dangling
    /// (refused by the walk), so nothing pinned that the final read follows
    /// the link — which it must, since containment was already decided on the
    /// canonical form. `#[cfg(unix)]`: creating a file symlink needs no
    /// privilege here, and Windows junctions cannot resolve onto a file.
    #[cfg(unix)]
    #[test]
    fn a_resolvable_symlink_leaf_is_read_through() {
        let mut f = fixture(Some("notes/a.md"));
        let link = f.root().join("link.md");
        std::os::unix::fs::symlink(f.root().join("notes/a.md"), &link).expect("symlink");

        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "link.md", "link.md")
            .expect("point the revision at the symlink");
        let expected = revision_hash(b"hello world");
        f.store
            .append_revision(&NewSourceRevision {
                ingested_at: 2,
                ..revision(&expected, Some("link.md"))
            })
            .expect("append the real revision");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, &expected);
        assert_eq!(
            v.status,
            VerificationStatus::Passed,
            "detail: {:?}",
            v.detail
        );
        assert_eq!(
            v.detail, None,
            "a readable link is read through and hashed like any other leaf"
        );
    }

    /// A `..` that happens to land back inside the workspace is still a
    /// malformed `local_path`: the column's contract is workspace-relative
    /// *and* `..`-free (ADR 0024), so the form check refuses it before
    /// containment is asked to forgive it.
    #[test]
    fn a_dot_dot_path_that_resolves_inside_is_still_refused() {
        let mut f = fixture(Some("notes/a.md"));
        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "notes/../notes/a.md", "a.md")
            .expect("point the revision at a .. path");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Unsupported);
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// A workspace root that is not there is the largest confirmed absence
    /// there is — every byte under it is gone. Reporting it `unsupported`
    /// instead would let an answer whose sources are all unverifiable come
    /// back `fully_verified()`.
    #[test]
    fn a_workspace_root_that_is_gone_reports_missing_bytes() {
        let f = fixture(Some("notes/a.md"));
        let gone = f.root().join("no-such-root");
        let source = StoreSource::new(&f.store, &gone);

        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            answer
                .citations
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
            "the chunks are in the store; only the bytes are unreachable"
        );
        assert!(
            !answer.fully_verified(),
            "an answer with no reachable source is not fully verified"
        );
    }

    /// A junction whose target has been removed — the end-to-end verdict: the
    /// entry exists and resolves nowhere, so the bytes named by the row are
    /// provably absent and the answer must not come back fully verified.
    ///
    /// The verdict is identical with and without the walk's refusal to
    /// re-append the name, so the refusal itself is pinned by
    /// `the_walk_refuses_an_entry_that_exists_but_does_not_resolve`
    /// (`#[cfg(windows)]`) and `the_walk_refuses_a_dangling_symlink`
    /// (`#[cfg(unix)]`); this fixture pins only the verdict. That the link is
    /// never *read through* is `a_junction_that_walks_out_of_the_workspace_is_
    /// not_read`'s claim — its target exists, so it reaches a different arm.
    #[cfg(windows)]
    #[test]
    fn a_dangling_junction_is_reported_as_source_missing() {
        let mut f = fixture(Some("notes/a.md"));
        let outside = tempfile::tempdir().expect("outside dir");
        let link = f.root().join("link");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(
            created,
            "this test needs to create a junction; a silent skip would make \
             \"did not run\" look exactly like \"passed\""
        );
        drop(outside); // remove the target: the junction is now dangling
        assert!(
            std::fs::canonicalize(&link).is_err(),
            "the junction must actually be dangling for this fixture to mean anything"
        );

        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", "link/secret.md", "secret.md")
            .expect("point the revision at a dangling link");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            !answer.fully_verified(),
            "an answer whose cited source resolves nowhere was never checked"
        );
    }

    /// A workspace root that is itself a dangling junction — the same
    /// predicate as a dangling `local_path`, and it must reach the same
    /// verdict: a confirmed absence, not "could not decide".
    #[cfg(windows)]
    #[test]
    fn a_workspace_root_that_is_a_dangling_link_reports_missing_bytes() {
        let f = fixture(Some("notes/a.md"));
        let outside = tempfile::tempdir().expect("outside dir");
        let link = f.workspace.path().join("rootlink");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(
            created,
            "this test needs to create a junction; a silent skip would make \
             \"did not run\" look exactly like \"passed\""
        );
        drop(outside);
        assert!(
            std::fs::canonicalize(&link).is_err(),
            "the root must actually be a dangling link for this fixture to mean anything"
        );

        let source = StoreSource::new(&f.store, &link);
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            !answer.fully_verified(),
            "an answer whose workspace root resolves nowhere is not fully verified"
        );
    }

    /// A *relative* workspace root is a supported input — ingest absolutizes
    /// its own inputs for the same reason. Without that step the walk bottoms
    /// out at `""`, `file_name()` is `None`, and the error makes an
    /// unreachable source look like an I/O failure: `unsupported`, which does
    /// **not** veto `fully_verified()`.
    #[test]
    fn a_relative_workspace_root_that_is_gone_reports_missing_bytes() {
        let f = fixture(Some("notes/a.md"));
        let source = StoreSource::new(&f.store, "no-such-relative-root");

        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(v.status, VerificationStatus::Failed);
        assert_eq!(v.detail, Some(VerifyDetail::SourceMissing));

        let answer =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            answer
                .citations
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
            "the chunks are in the store; only the bytes are unreachable"
        );
        assert!(
            !answer.fully_verified(),
            "an answer with no reachable source is not fully verified"
        );
    }

    /// The shape the CLI already builds its workspace root in
    /// (`apps/cli` takes it from `current_dir()`): with that root a
    /// drive-relative `local_path` resolves *inside* the workspace, so
    /// containment forgives it and only the form check can refuse it.
    #[cfg(windows)]
    #[test]
    fn a_drive_relative_local_path_under_a_cwd_shaped_root_is_still_refused() {
        let mut f = fixture(None);
        let cwd = std::env::current_dir().expect("cwd");
        let cwd_text = cwd.to_string_lossy();
        let drive = cwd_text
            .get(..2)
            .unwrap_or_else(|| panic!("expected a drive-lettered cwd, got {cwd_text}"));
        assert!(
            drive.ends_with(':'),
            "expected a drive letter, got {drive:?}"
        );
        let local_path = format!("{drive}Cargo.toml");

        // The premise the whole fixture rests on: containment *forgives* this
        // path (it resolves inside the workspace on the cwd's own drive), so
        // only the form check can refuse it. Without that, containment alone
        // would produce the verdict asserted below and CD10 would survive.
        let canonical_cwd = std::fs::canonicalize(&cwd).expect("canonical cwd");
        assert!(
            std::fs::canonicalize(cwd.join(&local_path))
                .is_ok_and(|resolved| resolved.starts_with(&canonical_cwd)),
            "premise: containment must forgive a drive-relative path under a cwd-shaped root"
        );

        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", &local_path, "Cargo.toml")
            .expect("point the revision at a drive-relative path");
        let source = StoreSource::new(&f.store, &cwd);
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(
            v.status,
            VerificationStatus::Unsupported,
            "the form check must refuse it even though containment would forgive it"
        );
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// A *rooted* path (no drive prefix) that resolves **inside** the
    /// workspace. `is_absolute()` is false for it on Windows, so the old form
    /// check let it through and containment forgave it — this is the one input
    /// that separates the form check from the canonicalize containment, and
    /// without it dropping the `RootDir` arm changes no observable verdict.
    #[cfg(windows)]
    #[test]
    fn a_rooted_path_that_resolves_inside_is_still_refused() {
        let mut f = fixture(None);
        let full = std::fs::canonicalize(f.root()).expect("canonical root");
        let verbatim = full.to_string_lossy();
        let unprefixed = verbatim
            .strip_prefix(r"\\?\")
            .unwrap_or_else(|| panic!("expected a verbatim canonical root, got {verbatim}"));
        let driveless = unprefixed
            .strip_prefix(|c: char| c.is_ascii_alphabetic())
            .and_then(|rest| rest.strip_prefix(':'))
            .unwrap_or_else(|| panic!("expected a drive letter in {unprefixed}"));
        assert!(
            driveless.starts_with('\\'),
            "expected a rooted body, got {driveless}"
        );
        let rooted = format!("{}{}", driveless.replace('\\', "/"), "/notes/gone.md");
        assert!(
            !Path::new(&rooted).is_absolute(),
            "on Windows a rooted path with no drive prefix is relative — \
             that is the whole point of this fixture"
        );
        // The premise: joining it keeps the base and lands *inside* this
        // workspace, so containment would forgive it and only the form check
        // can refuse it. Without this the fixture could pass for an
        // unrelated reason and would pin nothing about the `RootDir` arm.
        assert!(
            f.root().join(&rooted).starts_with(f.root()),
            "premise: a rooted path built from the workspace's own address \
             must resolve inside it"
        );

        f.store
            .update_revision_fingerprint(SOURCE_KEY, REVISION, "{}", &rooted, "gone.md")
            .expect("point the revision at a rooted path");
        let source = StoreSource::new(&f.store, f.root());
        let v = integrity(&source, SOURCE_KEY, REVISION);
        assert_eq!(
            v.status,
            VerificationStatus::Unsupported,
            "the form check must refuse it even though containment would forgive it"
        );
        assert_eq!(v.detail, Some(VerifyDetail::NoLocalBytes));
    }

    /// The walk must *refuse* — not re-append — an entry that exists but does
    /// not resolve. The verdict alone cannot show this: with or without the
    /// refusal the caller would reach `source_missing` anyway, and the point
    /// of the refusal is that `metadata` is never called through a broken
    /// link. So pin the walk itself.
    #[cfg(windows)]
    #[test]
    fn the_walk_refuses_an_entry_that_exists_but_does_not_resolve() {
        let ws = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let link = ws.path().join("link");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .is_ok_and(|output| output.status.success());
        assert!(
            created,
            "this test needs to create a junction; a silent skip would make \
             \"did not run\" look exactly like \"passed\""
        );
        drop(outside);

        assert_eq!(
            canonicalize_existing_ancestor(&link.join("secret.md")).expect("walk"),
            None,
            "under a broken link the walk must refuse, not re-append the name"
        );
        // A path that is merely absent is resolved through its existing
        // ancestor instead — that is what keeps a deleted file reportable.
        assert!(
            canonicalize_existing_ancestor(&ws.path().join("gone.md"))
                .expect("walk")
                .is_some(),
            "an absent file must still resolve through an existing ancestor"
        );
    }

    /// The missing tail comes back in **path order** — the property
    /// `momotaro-ingest`'s copy of this walk pins on its side. The two are
    /// deliberately different in shape (this one answers `Option` and refuses
    /// a broken link) and must not differ in order: a reversed tail makes a
    /// citation's `local_path` point somewhere else, silently.
    #[test]
    fn a_missing_tail_is_re_appended_in_path_order() {
        let ws = tempfile::tempdir().expect("temp workspace");
        std::fs::create_dir_all(ws.path().join("a")).expect("dir a");
        let missing = ws.path().join("a").join("b").join("c.md");
        let expected = std::fs::canonicalize(ws.path().join("a"))
            .expect("canonical a")
            .join("b")
            .join("c.md");
        assert_eq!(
            canonicalize_existing_ancestor(&missing).expect("walk"),
            Some(expected),
            "the missing tail keeps its order"
        );
    }

    /// The ubuntu CI leg's half of the same property.
    #[cfg(unix)]
    #[test]
    fn the_walk_refuses_a_dangling_symlink() {
        let ws = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let link = ws.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
        drop(outside);

        assert_eq!(
            canonicalize_existing_ancestor(&link.join("secret.md")).expect("walk"),
            None
        );
        assert!(
            canonicalize_existing_ancestor(&ws.path().join("gone.md"))
                .expect("walk")
                .is_some()
        );
    }

    /// The crate doc says this crate never writes. Pin it to bytes: the
    /// database and its write-ahead log must come out of a full judgement
    /// unchanged. (`-shm` is skipped — SQLite may rewrite its read index
    /// without any write on our part.)
    #[test]
    fn judging_leaves_the_store_bytes_untouched() {
        let f = fixture(Some("notes/a.md"));
        let db = f.workspace.path().join("momotaro.db");
        let wal = f.workspace.path().join("momotaro.db-wal");
        let before = (
            std::fs::read(&db).expect("read db"),
            std::fs::read(&wal).unwrap_or_default(),
        );

        let source = StoreSource::new(&f.store, f.root());
        let verdict =
            crate::verify_answer(&source, VerifyPolicy::enabled(), &["f0001"], &[citation()]);
        assert!(
            verdict
                .citations
                .iter()
                .all(|v| v.status == VerificationStatus::Passed),
            "the fixture must produce a passing citation for this to mean anything"
        );

        let after = (
            std::fs::read(&db).expect("read db"),
            std::fs::read(&wal).unwrap_or_default(),
        );
        assert_eq!(before, after, "verification must not write to the store");
    }
}
