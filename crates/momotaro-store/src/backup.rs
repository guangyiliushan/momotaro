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
    if let Err(error) = verify_artifact(&temp) {
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
}
