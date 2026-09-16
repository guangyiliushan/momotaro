//! SQLite-backed canonical storage for Momotaro.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};

use momotaro_contracts::{
    CURRENT_SCHEMA_VERSION, HealthReport, SchemaVersion, StatsReport, WorkspaceStatus,
};
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

pub mod chunks;
mod migration;
pub mod revisions;

pub use chunks::SourceCounts;
pub use revisions::NewSourceRevision;

const SCHEMA_TABLE: &str = "schema_meta";
const SCHEMA_VERSION_KEY: &str = "schema_version";

/// Storage-level failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite returned an error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database was created by a newer version of Momotaro.
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew {
        /// The version found in the database.
        found: u32,
        /// The highest version this build understands.
        supported: u32,
    },

    /// The database version is older and has no migration path yet.
    #[error("database schema version {found} requires migrations")]
    SchemaTooOld {
        /// The version found in the database.
        found: u32,
    },

    /// The stored version is not a valid unsigned integer.
    #[error("invalid schema version in database: {0}")]
    InvalidSchemaVersion(String),

    /// A stored column value does not decode into its contract enum.
    #[error("invalid value in column {column}: {value}")]
    InvalidValue {
        /// The column that failed to decode.
        column: &'static str,
        /// The raw value found in the column.
        value: String,
    },
}

pub(crate) fn invalid_value(column: &'static str, value: impl Into<String>) -> StoreError {
    StoreError::InvalidValue {
        column,
        value: value.into(),
    }
}

/// An open SQLite store.
pub struct Store {
    connection: Connection,
    path: PathBuf,
}

impl Store {
    /// Opens or creates the SQLite database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path)?;

        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;

        Ok(Self { connection, path })
    }

    /// Returns the canonical database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the initial schema if missing, then applies pending migrations.
    ///
    /// Each migration runs in its own transaction together with its schema
    /// version bump, so the stored version always matches the applied DDL.
    pub fn init_schema(&mut self) -> Result<(), StoreError> {
        self.connection.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {SCHEMA_TABLE} (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        ))?;

        let version = self.schema_version()?.map(|SchemaVersion(found)| found);

        if let Some(found) = version
            && found > CURRENT_SCHEMA_VERSION
        {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        migration::apply_pending(&mut self.connection, version.unwrap_or(0))
    }

    /// Reads the stored schema version, or `None` for an uninitialized store.
    pub fn schema_version(&self) -> Result<Option<SchemaVersion>, StoreError> {
        let table_exists = self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [SCHEMA_TABLE],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        if !table_exists {
            return Ok(None);
        }

        let value = self
            .connection
            .query_row(
                &format!("SELECT value FROM {SCHEMA_TABLE} WHERE key = ?1"),
                [SCHEMA_VERSION_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let Some(value) = value else {
            return Ok(None);
        };

        let version = value
            .parse::<u32>()
            .map_err(|_| StoreError::InvalidSchemaVersion(value))?;

        Ok(Some(SchemaVersion(version)))
    }

    /// Returns non-mutating health facts.
    pub fn health(&self) -> Result<HealthReport, StoreError> {
        match self.schema_version()? {
            Some(SchemaVersion(CURRENT_SCHEMA_VERSION)) => Ok(HealthReport {
                status: WorkspaceStatus::Ready,
                schema_version: Some(SchemaVersion::CURRENT),
                message: None,
            }),
            Some(SchemaVersion(found)) if found > CURRENT_SCHEMA_VERSION => Ok(HealthReport {
                status: WorkspaceStatus::Invalid,
                schema_version: Some(SchemaVersion(found)),
                message: Some(format!(
                    "database schema version {found} is newer than supported {CURRENT_SCHEMA_VERSION}"
                )),
            }),
            Some(SchemaVersion(found)) => Ok(HealthReport {
                status: WorkspaceStatus::Invalid,
                schema_version: Some(SchemaVersion(found)),
                message: Some(format!(
                    "database schema version {found} requires migrations"
                )),
            }),
            None => Ok(HealthReport {
                status: WorkspaceStatus::Uninitialized,
                schema_version: None,
                message: Some("database has not been initialized".to_owned()),
            }),
        }
    }

    /// Returns a small local stats projection.
    ///
    /// Status derivation mirrors [`Store::health`]: only the current schema
    /// version is `Ready`; older versions are `Invalid` (they need
    /// migrations, not re-initialization), and a missing version row means
    /// `Uninitialized`. Counts are read only when the canonical tables are
    /// known to exist.
    pub fn stats(&self) -> Result<StatsReport, StoreError> {
        let schema_version = self.schema_version()?;
        let (status, initialized) = match &schema_version {
            Some(SchemaVersion(found)) if *found == CURRENT_SCHEMA_VERSION => {
                (WorkspaceStatus::Ready, true)
            }
            Some(SchemaVersion(found)) if *found > CURRENT_SCHEMA_VERSION => {
                (WorkspaceStatus::Invalid, false)
            }
            Some(SchemaVersion(_)) => (WorkspaceStatus::Invalid, false),
            None => (WorkspaceStatus::Uninitialized, false),
        };

        let (sources, chunks) = if initialized {
            let counts = self.counts()?;
            (Some(counts.sources), Some(counts.chunks))
        } else {
            (None, None)
        };

        Ok(StatsReport {
            status,
            schema_version,
            database_path: self.path.clone(),
            initialized,
            sources,
            chunks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use momotaro_contracts::{Chunk, OriginClass, SourceKind};

    fn open_temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = Store::open(dir.path().join("momotaro.db")).expect("open store");
        (dir, store)
    }

    #[test]
    fn future_schema_is_rejected() {
        let (_dir, mut store) = open_temp_store();
        store
            .connection
            .execute_batch(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta (key, value) VALUES ('schema_version', '999');",
            )
            .expect("write future schema");

        let error = store.init_schema().expect_err("future schema must fail");
        assert!(matches!(error, StoreError::SchemaTooNew { .. }));
    }

    #[test]
    fn init_creates_current_schema() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("initialize schema");
        assert_eq!(
            store.schema_version().expect("read schema"),
            Some(SchemaVersion::CURRENT)
        );
    }

    #[test]
    fn fresh_db_lands_at_version_two_with_tables_and_indexes() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("initialize schema");

        let tables: Vec<String> = store
            .connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .and_then(|mut s| s.query_map([], |row| row.get(0))?.collect())
            .expect("list tables");
        assert!(tables.contains(&"source_revisions".to_owned()));
        assert!(tables.contains(&"chunks".to_owned()));

        let indexes: Vec<String> = store
            .connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%'")
            .and_then(|mut s| s.query_map([], |row| row.get(0))?.collect())
            .expect("list indexes");
        assert!(indexes.contains(&"idx_chunks_rev".to_owned()));
        assert!(indexes.contains(&"idx_revisions_current".to_owned()));
    }

    #[test]
    fn v1_database_migrates_forward_and_keeps_rows() {
        let (_dir, mut store) = open_temp_store();
        store
            .connection
            .execute_batch(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta (key, value) VALUES ('schema_version', '1');
                 INSERT INTO schema_meta (key, value) VALUES ('sentinel', 'keep-me');",
            )
            .expect("create v1 baseline");

        store.init_schema().expect("migrate to current");

        assert_eq!(
            store.schema_version().expect("read schema"),
            Some(SchemaVersion::CURRENT)
        );
        let sentinel: String = store
            .connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'sentinel'",
                [],
                |row| row.get(0),
            )
            .expect("sentinel survives migration");
        assert_eq!(sentinel, "keep-me");
    }

    #[test]
    fn health_reports_uninitialized_store() {
        let (_dir, store) = open_temp_store();
        let report = store.health().expect("health report");
        assert_eq!(report.status, WorkspaceStatus::Uninitialized);
        assert_eq!(report.schema_version, None);
    }

    #[test]
    fn init_is_idempotent() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("initialize schema");
        store.init_schema().expect("initialize schema again");
        assert_eq!(
            store.schema_version().expect("read schema"),
            Some(SchemaVersion::CURRENT)
        );
    }

    #[test]
    fn stats_reports_zeroes_when_initialized_empty() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("initialize schema");

        let report = store.stats().expect("stats");
        assert!(report.initialized);
        assert_eq!(report.sources, Some(0));
        assert_eq!(report.chunks, Some(0));
    }

    /// A database written by a NEWER build must surface as `Invalid`, not
    /// `Uninitialized` — it exists and must not be silently re-initialized.
    #[test]
    fn stats_reports_future_schema_as_invalid() {
        let (_dir, store) = open_temp_store();
        store
            .connection
            .execute_batch(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta (key, value) VALUES ('schema_version', '999');",
            )
            .expect("write future schema");

        let report = store.stats().expect("stats");
        assert_eq!(report.status, WorkspaceStatus::Invalid);
        assert!(!report.initialized);
        assert_eq!(report.sources, None);
        assert_eq!(
            report.schema_version,
            Some(SchemaVersion(999)),
            "version must be surfaced, not collapsed"
        );
    }

    /// An older, migration-capable database is `Invalid` ("needs
    /// migrations"), aligned with `health()`.
    #[test]
    fn stats_reports_old_schema_as_invalid_not_uninitialized() {
        let (_dir, store) = open_temp_store();
        store
            .connection
            .execute_batch(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta (key, value) VALUES ('schema_version', '1');",
            )
            .expect("write v1 schema");

        let report = store.stats().expect("stats");
        assert_eq!(report.status, WorkspaceStatus::Invalid);
        assert!(!report.initialized);
        assert_eq!(report.schema_version, Some(SchemaVersion(1)));
    }

    #[test]
    fn stats_fills_counts_after_ingest() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("initialize schema");

        store
            .append_revision(&NewSourceRevision {
                source_key: "a.md",
                revision_hash: "a1",
                kind: SourceKind::Note,
                title: Some("A"),
                uri: None,
                local_path: None,
                origin_class: OriginClass::Owner,
                metadata_json: "{}",
                ingested_at: 1,
            })
            .expect("append a1");
        store
            .append_revision(&NewSourceRevision {
                source_key: "a.md",
                revision_hash: "a2",
                kind: SourceKind::Note,
                title: Some("A"),
                uri: None,
                local_path: None,
                origin_class: OriginClass::Owner,
                metadata_json: "{}",
                ingested_at: 2,
            })
            .expect("append a2");

        let chunks = vec![
            Chunk {
                chunk_id: "c1".to_owned(),
                source_key: "a.md".to_owned(),
                revision_hash: "a2".to_owned(),
                ordinal: 0,
                heading_path: Some("H".to_owned()),
                text: "alpha".to_owned(),
                locator_json: "{}".to_owned(),
                chunk_hash: "h1".to_owned(),
                token_estimate: 1,
            },
            Chunk {
                chunk_id: "c2".to_owned(),
                source_key: "a.md".to_owned(),
                revision_hash: "a2".to_owned(),
                ordinal: 1,
                heading_path: None,
                text: "beta".to_owned(),
                locator_json: "{}".to_owned(),
                chunk_hash: "h2".to_owned(),
                token_estimate: 1,
            },
        ];
        store
            .replace_chunks("a.md", "a2", &chunks)
            .expect("replace chunks");

        let report = store.stats().expect("stats");
        assert!(report.initialized);
        assert_eq!(report.sources, Some(1));
        assert_eq!(report.chunks, Some(2));
    }
}
