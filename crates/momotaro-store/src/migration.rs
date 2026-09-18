//! Sequential forward-only schema migrations.
//!
//! Every step is exactly one transaction and the version stamp
//! (`PRAGMA user_version`) is the last statement inside it (D46): a crash
//! mid-migration rolls back to the previous consistent version instead of
//! leaving a half-migrated database.

use rusqlite::Connection;

use crate::StoreError;

/// Ordered forward-only migration steps. Applying every entry with a version
/// greater than the stored version brings the database to the newest schema.
const MIGRATIONS: &[(u32, &str)] = &[
    // 0 -> 1 baseline: `schema_meta` is created by `init_schema` before the
    // runner starts, so this step only records the version.
    (1, ""),
    // 1 -> 2: canonical source revisions and chunks.
    (
        2,
        "CREATE TABLE IF NOT EXISTS source_revisions (
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  kind TEXT NOT NULL,
  title TEXT,
  uri TEXT,
  local_path TEXT,
  origin_class TEXT NOT NULL,
  metadata_json TEXT NOT NULL,
  ingested_at INTEGER NOT NULL,
  is_current INTEGER NOT NULL,
  PRIMARY KEY (source_key, revision_hash)
);
CREATE TABLE IF NOT EXISTS chunks (
  chunk_id TEXT PRIMARY KEY,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  heading_path TEXT,
  text TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  chunk_hash TEXT NOT NULL,
  token_estimate INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chunks_rev ON chunks(source_key, revision_hash);
CREATE INDEX IF NOT EXISTS idx_revisions_current ON source_revisions(source_key) WHERE is_current = 1;",
    ),
    // 2 -> 3: key scheme v2 — the raw spelling of a name beside its key.
    (3, "ALTER TABLE source_revisions ADD COLUMN raw_name TEXT;"),
];

/// Applies every migration newer than `version`.
pub(crate) fn apply_pending(connection: &mut Connection, version: u32) -> Result<(), StoreError> {
    for &(target, sql) in MIGRATIONS {
        if target > version {
            apply_step(connection, target, sql)?;
        }
    }

    Ok(())
}

/// Applies one step: all of its SQL and the version stamp in a single
/// transaction, with the stamp written last (D46). A failure rolls the whole
/// step back and leaves the previous version in place.
fn apply_step(connection: &mut Connection, target: u32, sql: &str) -> Result<(), StoreError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(sql)?;
    // The version stamp is the last statement in the transaction (D46).
    transaction.execute_batch(&format!("PRAGMA user_version = {target};"))?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[test]
    fn every_step_is_ordered_and_unique() {
        let mut seen = Vec::new();
        for &(target, _) in MIGRATIONS {
            assert!(
                seen.last().is_none_or(|previous| previous < &target),
                "migration targets must strictly increase: {target}"
            );
            seen.push(target);
        }
        assert_eq!(seen.first(), Some(&1), "the chain starts at 1");
    }

    #[test]
    fn a_failing_step_rolls_back_and_keeps_the_previous_version() {
        let mut connection = Connection::open_in_memory().expect("open in-memory store");
        apply_step(&mut connection, 1, "CREATE TABLE one (x);").expect("first step succeeds");

        // The step creates a table and then fails: the table must vanish and
        // the version must stay where the last successful step left it (D46).
        apply_step(
            &mut connection,
            2,
            "CREATE TABLE two (x); CREATE TABLE two (x);",
        )
        .expect_err("the second statement cannot succeed");

        assert_eq!(
            version(&connection),
            1,
            "the stamp stays at the last good step"
        );
        assert!(
            table_exists(&connection, "one"),
            "the earlier step survives"
        );
        assert!(
            !table_exists(&connection, "two"),
            "the failed step's DDL is rolled back"
        );
    }

    fn version(connection: &Connection) -> u32 {
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version")
    }

    fn table_exists(connection: &Connection, name: &str) -> bool {
        connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |_| Ok(()),
            )
            .optional()
            .expect("query sqlite_master")
            .is_some()
    }
}
