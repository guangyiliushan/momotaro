//! Pre-migration backups: `VACUUM INTO` → verify → atomic rename.
//!
//! Step order follows D32 / D46: write to a temp name, verify the artifact with
//! `quick_check`, then publish by atomic rename. An interrupted run only wastes
//! an artifact and never touches the live database.
//!
//! Not implemented here, and not silently assumed: the D32 snapshot manifest
//! belongs to the snapshot flow (desktop → mobile), which has no consumer for a
//! pre-migration copy yet; and the disk-space pre-check is left to `VACUUM INTO`
//! itself failing, which surfaces as a failed backup — fail-closed, no skip
//! option (§8.2).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::StoreError;

/// File-name prefix of an unpublished backup artifact.
const TMP_PREFIX: &str = ".tmp-backup-";

/// Runs the four-step backup and returns the published snapshot path.
pub(crate) fn backup_before_migration(
    connection: &Connection,
    dir: &Path,
    from: u32,
    to: u32,
) -> Result<PathBuf, StoreError> {
    backup_before_migration_with(connection, dir, from, to, verify_artifact)
}

/// The four-step backup with the artifact check injected.
///
/// The seam exists so tests can drive the fail-closed wiring itself — that a
/// refused artifact is never published and leaves nothing behind — while
/// [`verify_artifact`] keeps covering what "verified" means.
pub(crate) fn backup_before_migration_with(
    connection: &Connection,
    dir: &Path,
    from: u32,
    to: u32,
    verify: impl FnOnce(&Path) -> Result<(), StoreError>,
) -> Result<PathBuf, StoreError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let target = format!("backup-v{from}-to-v{to}-{stamp}.db");
    let temp = dir.join(format!("{TMP_PREFIX}{target}"));
    let published = dir.join(&target);

    if temp.exists() {
        fs::remove_file(&temp).map_err(|error| StoreError::BackupFailed(error.to_string()))?;
    }

    // `VACUUM INTO` refuses an existing target and must run outside a
    // transaction; it always produces a transactionally consistent snapshot.
    connection
        .execute("VACUUM INTO ?1", [temp.to_string_lossy().as_ref()])
        .map_err(|error| StoreError::BackupFailed(format!("vacuum into: {error}")))?;

    // Verify the artifact before it may replace anything: `quick_check` reads
    // the whole file, which is also the first corruption screening (D32 L-backup).
    if let Err(error) = verify(&temp) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    fs::rename(&temp, &published).map_err(|error| {
        let _ = fs::remove_file(&temp);
        StoreError::BackupFailed(format!("publish {}: {error}", published.display()))
    })?;
    Ok(published)
}

/// Verifies a backup artifact: it must open read-only and pass `quick_check`.
///
/// `quick_check` reads the whole file, which is also the first corruption
/// screening of the snapshot (D32 L-backup). Kept separate from the four-step
/// so its teeth can be tested without manufacturing a broken `VACUUM INTO`.
pub(crate) fn verify_artifact(path: &Path) -> Result<(), StoreError> {
    let artifact = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| StoreError::BackupFailed(format!("open artifact: {error}")))?;
    let verdict: String = artifact
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| StoreError::BackupFailed(format!("quick_check: {error}")))?;
    if verdict != "ok" {
        return Err(StoreError::BackupFailed(format!("quick_check: {verdict}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn a_failed_verification_publishes_nothing_and_cleans_up() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");

        // The wiring itself: a refused artifact must fail the backup before
        // anything carries the backup name, and the artifact must not linger.
        // Removing the fail-closed branch leaves a published artifact behind
        // and turns this test red.
        let error = backup_before_migration_with(&store.connection, dir.path(), 2, 3, |_path| {
            Err(StoreError::BackupFailed(
                "injected: artifact refused".to_owned(),
            ))
        })
        .expect_err("a refused artifact must fail the backup");
        assert!(matches!(error, StoreError::BackupFailed(_)), "{error}");

        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("backup-") || name.starts_with(TMP_PREFIX))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a refused artifact is neither published nor left behind: {leftovers:?}"
        );
        assert!(path.exists(), "the live database is untouched");
    }

    #[test]
    fn the_artifact_is_verified_before_it_is_published() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");

        // The order half: the verifier must see the *unpublished* artifact —
        // its temp name is the proof that nothing carries the trusted backup
        // name before verification. Swapping publish ahead of verification
        // hands the verifier the published name and turns this test red.
        let mut seen: Vec<String> = Vec::new();
        let snapshot =
            backup_before_migration_with(&store.connection, dir.path(), 2, 3, |artifact| {
                seen.push(
                    artifact
                        .file_name()
                        .expect("artifact name")
                        .to_string_lossy()
                        .into_owned(),
                );
                Ok(())
            })
            .expect("backup succeeds");
        assert_eq!(seen.len(), 1, "the verifier runs exactly once");
        assert!(
            seen[0].starts_with(TMP_PREFIX),
            "verification runs on the unpublished temp artifact, not on the published name: {seen:?}"
        );
        assert!(
            snapshot.exists(),
            "and the snapshot is published afterwards"
        );
    }

    #[test]
    fn backup_publishes_a_verified_snapshot_and_leaves_the_original() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");

        let snapshot =
            backup_before_migration(&store.connection, dir.path(), 2, 3).expect("backup succeeds");
        assert!(snapshot.exists(), "snapshot is published");
        assert!(path.exists(), "the live database is untouched");
        assert!(
            !snapshot
                .file_name()
                .expect("snapshot name")
                .to_string_lossy()
                .starts_with(TMP_PREFIX),
            "the temp name never survives publication"
        );

        let artifact = rusqlite::Connection::open(&snapshot).expect("open snapshot");
        let verdict: String = artifact
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .expect("quick_check");
        assert_eq!(verdict, "ok");
    }

    #[test]
    fn verification_rejects_a_damaged_artifact() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // 1. Not a database at all: opening read-only succeeds, reading it does
        //    not, so the verification must fail closed.
        let garbage = dir.path().join("garbage.db");
        fs::write(&garbage, b"this is not a sqlite database, not even close").expect("write");
        assert!(
            matches!(verify_artifact(&garbage), Err(StoreError::BackupFailed(_))),
            "a file that is not a database must not verify"
        );

        // 2. A real snapshot with a corrupted first page: the header still
        //    parses, the body does not.
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");
        let corrupt = dir.path().join("corrupt.db");
        fs::copy(&path, &corrupt).expect("copy the live database");
        let mut bytes = fs::read(&corrupt).expect("read copy");
        assert!(bytes.len() > 200, "the copy has pages to damage");
        for byte in bytes.iter_mut().skip(100).take(500) {
            *byte = 0xFF;
        }
        fs::write(&corrupt, &bytes).expect("write damaged copy");
        assert!(
            matches!(verify_artifact(&corrupt), Err(StoreError::BackupFailed(_))),
            "a damaged snapshot must not verify"
        );

        // 3. And the verifier is not simply always failing.
        let good = backup_before_migration(&store.connection, dir.path(), 1, 2).expect("snapshot");
        verify_artifact(&good).expect("a real snapshot verifies");
    }

    #[test]
    fn a_backup_that_cannot_be_written_fails_closed() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");

        // distribution-trust §8.2: a backup that cannot be produced means the
        // migration refuses to start, with no skip option.
        let missing = dir.path().join("gone-away");
        let error = backup_before_migration(&store.connection, &missing, 2, 3)
            .expect_err("a missing directory must fail the backup");
        assert!(matches!(error, StoreError::BackupFailed(_)), "{error}");
        assert!(!missing.exists(), "nothing is created at the blocked path");
        assert!(path.exists(), "the live database is untouched");
    }

    #[test]
    fn an_existing_target_is_replaced_only_after_a_new_artifact_exists() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");

        backup_before_migration(&store.connection, dir.path(), 1, 2).expect("first backup");
        let second = backup_before_migration(&store.connection, dir.path(), 1, 2).expect("second");
        assert!(second.exists());

        // Two runs in the same second share a stamp (the second artifact then
        // replaces the first); crossing a second boundary leaves two. Either
        // way each artifact on disk is complete — the property under test.
        let artifacts: Vec<std::path::PathBuf> = fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|entry| {
                entry
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("backup-v1-to-v2-"))
            })
            .collect();
        assert!(
            (1..=2).contains(&artifacts.len()),
            "at most one artifact per second: {artifacts:?}"
        );
        for artifact in &artifacts {
            verify_artifact(artifact).expect("every published artifact verifies");
        }
        assert!(path.exists(), "the live database is untouched");
    }

    /// The name of `an_existing_target_is_replaced_only_after_a_new_artifact_exists`
    /// promises a failure-safety order: a published artifact is replaced only
    /// once a new one exists **and verifies**. A second backup that fails
    /// mid-flight (here: the injected `verify` refuses) must leave the first
    /// artifact in place and still verifying — deleting the old one before
    /// the new one is safe would lose the only good backup on a half-run.
    #[test]
    fn a_failed_second_backup_leaves_the_first_artifact_in_place() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("init schema");

        let first =
            backup_before_migration(&store.connection, dir.path(), 1, 2).expect("first backup");
        verify_artifact(&first).expect("the first artifact verifies");

        let error = backup_before_migration_with(&store.connection, dir.path(), 1, 2, |_| {
            Err(StoreError::BackupFailed(
                "injected verify refusal".to_string(),
            ))
        })
        .expect_err("the refused second backup must fail");
        assert!(matches!(error, StoreError::BackupFailed(_)), "{error}");

        assert!(
            first.exists(),
            "the first artifact survives a failed second backup"
        );
        verify_artifact(&first).expect("the surviving artifact still verifies");
        assert!(path.exists(), "the live database is untouched");
    }
}
