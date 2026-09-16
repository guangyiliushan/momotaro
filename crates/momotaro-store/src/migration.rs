//! Sequential forward-only schema migrations.

use rusqlite::{Connection, params};

use crate::{SCHEMA_TABLE, SCHEMA_VERSION_KEY, StoreError};

/// Ordered forward-only migration steps. Applying every entry with a version
/// greater than the stored version brings the database to the newest schema.
const MIGRATIONS: &[(u32, &str)] = &[
    // 0 -> 1 baseline: `schema_meta` is created by `init_schema` before the
    // runner starts, so this step only records the version.
    (1, ""),
    // 1 -> 2: canonical source revisions and chunks.
    (
        2,
        "CREATE TABLE source_revisions (
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
CREATE TABLE chunks (
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
CREATE INDEX idx_chunks_rev ON chunks(source_key, revision_hash);
CREATE INDEX idx_revisions_current ON source_revisions(source_key) WHERE is_current = 1;",
    ),
];

/// Applies every migration newer than `version`, one transaction per step.
pub(crate) fn apply_pending(connection: &mut Connection, version: u32) -> Result<(), StoreError> {
    for &(target, sql) in MIGRATIONS {
        if target <= version {
            continue;
        }

        let tx = connection.transaction()?;
        tx.execute_batch(sql)?;
        // Upsert so a fresh database (no version row yet) records its baseline.
        tx.execute(
            &format!(
                "INSERT INTO {SCHEMA_TABLE} (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value"
            ),
            params![SCHEMA_VERSION_KEY, target.to_string()],
        )?;
        tx.commit()?;
    }

    Ok(())
}
