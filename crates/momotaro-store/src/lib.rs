//! SQLite-backed canonical storage for Momotaro.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};

use momotaro_contracts::{
    CURRENT_SCHEMA_VERSION, HealthReport, SchemaVersion, SqliteFacts, StatsReport, WorkspaceStatus,
};
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

mod backup;
pub mod chunks;
pub mod events;
mod graph;
mod migration;
pub mod revisions;

pub use chunks::SourceCounts;
pub use events::{NewStoreEvent, StoreEvent};
pub use revisions::NewSourceRevision;

/// Table holding multi-value store metadata. Never the schema version: that
/// lives in `PRAGMA user_version` (D46).
const SCHEMA_TABLE: &str = "schema_meta";

/// The version key a pre-`user_version` build wrote into `schema_meta`. Kept
/// only to *detect* such stores so they are refused with a clear message
/// instead of being half-migrated.
const LEGACY_SCHEMA_VERSION_KEY: &str = "schema_version";

/// File identity written to `PRAGMA application_id` so opening a foreign
/// SQLite file fails loudly instead of silently creating tables inside it.
/// The value is ASCII `MMOT`; it is not one SQLite reserves for itself.
pub const APPLICATION_ID: i32 = 0x4D4D_4F54;

// SQLite keeps the application id in a *signed* 32-bit header field, and
// values at or above 2^31 read back as 0 — which would silently disable the
// identity check this constant exists for. Asserted at compile time so the
// guarantee cannot be lost by editing the constant alone.
const _: () = assert!(APPLICATION_ID > 0 && (APPLICATION_ID as u32) < (1u32 << 31));

/// Identity of the source-key scheme this build writes (D42 / ADR 0002).
/// Bumping it means the keys of an existing store are a different species.
pub const KEY_SCHEME: &str = "v2";

/// Storage-level failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite returned an error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A `graph_links` row this build cannot read: a vocabulary spelling from
    /// a newer one (ADR 0027 rows are typed at both ends).
    #[error("graph_links row has unknown {column} spelling {value:?}")]
    UnknownGraphSpelling {
        /// Which vocabulary column the spelling sits in.
        column: &'static str,
        /// The spelling found.
        value: String,
    },

    /// The database was created by a newer version of Momotaro.
    #[error("database schema version {found} is newer than supported {supported}")]
    SchemaTooNew {
        /// The version found in the database.
        found: u32,
        /// The highest version this build understands.
        supported: u32,
    },

    /// The stored version is not a valid unsigned integer.
    #[error("invalid schema version in database: {0}")]
    InvalidSchemaVersion(String),

    /// The file is a SQLite database, but not a Momotaro store.
    #[error("database application_id {found} is not a Momotaro store")]
    ForeignDatabase {
        /// The application id found in the file header.
        found: i32,
    },

    /// The file carries a schema version but no Momotaro identity: every build
    /// that ever wrote a version carrier also stamped `application_id` (both
    /// arrived in the same change), so this is somebody else's database that
    /// happens to use the version pragma.
    #[error(
        "database has no Momotaro identity but claims schema version {found}; refusing to write"
    )]
    NoIdentity {
        /// The version the file claims.
        found: u32,
    },

    /// The file has no identity of ours but already holds objects this build
    /// did not create: somebody else's plain SQLite database, not a store to
    /// adopt.
    #[error("database holds objects this build did not create ({objects}); refusing to write")]
    ForeignObjects {
        /// The offending object names, comma separated.
        objects: String,
    },

    /// A refresh named a revision that is not in the store.
    #[error("no revision {revision_hash} for source {source_key}")]
    RevisionMissing {
        /// Source key the caller asked for.
        source_key: String,
        /// Revision hash the caller asked for.
        revision_hash: String,
    },

    /// The store was written before `PRAGMA user_version` became the version
    /// carrier, so its rows may hold pre-scheme source keys.
    #[error(
        "store predates the user_version carrier (recorded schema version {found}); \
         delete the workspace data directory and re-index"
    )]
    LegacyStore {
        /// The version that legacy carrier recorded.
        found: String,
    },

    /// A pre-migration backup failed, so the migration never started (D46).
    #[error("pre-migration backup failed: {0}")]
    BackupFailed(String),

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

/// Facts about the SQLite engine this build links.
///
/// Read through SQL on a scratch in-memory connection — never ffi or `unsafe` —
/// so `doctor` can report them even when the workspace database is missing,
/// unreadable, or belongs to somebody else. That is exactly when the engine
/// version matters most.
pub fn sqlite_facts() -> Result<SqliteFacts, StoreError> {
    let connection = Connection::open_in_memory()?;
    let version: String = connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    let source_id: String =
        connection.query_row("SELECT sqlite_source_id()", [], |row| row.get(0))?;
    let compile_options = {
        let mut statement = connection.prepare("PRAGMA compile_options")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let version_number = rusqlite::version_number();

    Ok(SqliteFacts {
        version,
        version_number,
        source_id,
        compile_options,
        wal_reset_window: momotaro_contracts::is_wal_reset_window(version_number),
        withdrawn_release: momotaro_contracts::is_withdrawn_release(version_number),
    })
}

/// An open SQLite store.
pub struct Store {
    connection: Connection,
    path: PathBuf,
}

impl Store {
    /// Opens or creates the SQLite database at `path`.
    ///
    /// The engine-discipline PRAGMAs are applied only once the file is known to
    /// be ours or brand new: `journal_mode = WAL` rewrites the file header, so
    /// applying it to somebody else's database would be exactly the pollution
    /// `application_id` exists to prevent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path)?;

        // Connection-scoped settings: safe on any file, never persisted.
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA trusted_schema = OFF;",
        )?;

        let store = Self { connection, path };
        // The file-modifying PRAGMAs are gated by the *same* predicate that
        // `init_schema` refuses on: one answer to "may we write here?" means the
        // gate and the refusal set cannot drift into weaker copies of each other.
        if store.refuse_write().is_ok() {
            store.connection.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA cache_size = -65536;
                 PRAGMA journal_size_limit = 67108864;",
            )?;
        }
        Ok(store)
    }

    /// The single answer to "may this build write into this file?".
    ///
    /// Returns the refusal to report, or `Ok(())` when writing is allowed.
    /// [`Store::open`] consults it before applying file-modifying PRAGMAs and
    /// [`Store::init_schema`] reports its error — one predicate, so the two can
    /// never disagree about which files are ours.
    fn refuse_write(&self) -> Result<(), StoreError> {
        let found_id = self.application_id()?;
        if found_id != 0 && found_id != APPLICATION_ID {
            return Err(StoreError::ForeignDatabase { found: found_id });
        }

        let version = self.user_version()?;
        if version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        // A store written before `user_version` became the carrier cannot be
        // migrated safely: its rows carry pre-scheme keys, and mixing two key
        // species in one database is silent corruption (Q2). Checked before the
        // object test so a real legacy store gets this message rather than
        // "contains objects we did not create".
        if version == 0
            && let Some(legacy) = self.legacy_schema_version()?
        {
            return Err(StoreError::LegacyStore { found: legacy });
        }

        // Identity is `application_id`: this build stamps it before anything else,
        // so every build that ever wrote a version carrier also wrote the id (both
        // arrived in the same change). A file that carries a version but no id is
        // therefore not ours — and neither is one that holds objects we did not
        // create. Both are judged by contents, and both are refused.
        if found_id == 0 {
            if version > 0 {
                return Err(StoreError::NoIdentity { found: version });
            }
            let objects = self.foreign_objects()?;
            if !objects.is_empty() {
                return Err(StoreError::ForeignObjects {
                    objects: objects.join(", "),
                });
            }
        }

        Ok(())
    }

    /// Returns the canonical database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the initial schema if missing, then applies pending migrations.
    ///
    /// Order is the contract (D46): identity check → version read →
    /// `schema_meta` and the file-identity stamp (idempotent writes on every
    /// store this build can adopt) → verified backup of an existing store →
    /// migrations → key-scheme stamp. The load-bearing clause is the backup
    /// strictly before any *migration* DDL
    /// (`a_blocked_backup_stops_the_migration_before_any_ddl`); the last step
    /// is pinned by `a_migration_failure_leaves_the_key_scheme_unstamped`.
    pub fn init_schema(&mut self) -> Result<(), StoreError> {
        // Refusals first, writes later — decided by the *same* predicate
        // `Store::open` consults before touching the file header.
        self.refuse_write()?;

        let found_id = self.application_id()?;
        let version = self.user_version()?;

        // Every possible refusal is behind us: this is the first write of the
        // whole function.
        self.connection.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {SCHEMA_TABLE} (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        ))?;

        // The file is now known to be a store of ours or brand new, so stamp
        // its identity last: a refused file must never carry our marker.
        if found_id == 0 {
            self.connection
                .execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};"))?;
        }

        // D46: an existing store is backed up and verified before any migration
        // runs. A brand-new file has nothing to lose.
        if version > 0 && version < CURRENT_SCHEMA_VERSION {
            let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
            backup::backup_before_migration(
                &self.connection,
                dir,
                version,
                CURRENT_SCHEMA_VERSION,
            )?;
        }

        migration::apply_pending(&mut self.connection, version)?;
        self.set_metadata("key_scheme", KEY_SCHEME)?;
        Ok(())
    }

    /// Reads the schema version from `PRAGMA user_version`; `0` means "no
    /// schema yet".
    pub fn schema_version(&self) -> Result<Option<SchemaVersion>, StoreError> {
        let version = self.user_version()?;
        Ok((version > 0).then_some(SchemaVersion(version)))
    }

    /// Reads the raw `PRAGMA user_version` (the authoritative carrier, D46).
    pub fn user_version(&self) -> Result<u32, StoreError> {
        let value: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        u32::try_from(value).map_err(|_| StoreError::InvalidSchemaVersion(value.to_string()))
    }

    /// Reads `PRAGMA application_id`.
    pub fn application_id(&self) -> Result<i32, StoreError> {
        Ok(self
            .connection
            .query_row("PRAGMA application_id", [], |row| row.get(0))?)
    }

    /// Reads one `schema_meta` value.
    pub fn metadata(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .connection
            .query_row(
                &format!("SELECT value FROM {SCHEMA_TABLE} WHERE key = ?1"),
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Upserts one `schema_meta` value.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.connection.execute(
            &format!(
                "INSERT INTO {SCHEMA_TABLE} (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value"
            ),
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Everything in the file that this build did not create — SQLite's own
    /// `sqlite_%` bookkeeping is the only thing an adoptable file may contain.
    ///
    /// Deliberately *not* a name whitelist: that would be a second, weaker copy
    /// of "is this our store?", and a file that merely happens to contain a
    /// table called `chunks` is still somebody else's file.
    fn foreign_objects(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        Ok(statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether a table exists — read-only, so asking never creates one.
    fn table_exists(&self, name: &str) -> Result<bool, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The version a pre-`user_version` build recorded in `schema_meta`, if any.
    ///
    /// Read-only: the table is only queried when it already exists.
    fn legacy_schema_version(&self) -> Result<Option<String>, StoreError> {
        if !self.table_exists(SCHEMA_TABLE)? {
            return Ok(None);
        }
        self.metadata(LEGACY_SCHEMA_VERSION_KEY)
    }

    /// Returns non-mutating health facts.
    pub fn health(&self) -> Result<HealthReport, StoreError> {
        let schema_version = self.schema_version()?;

        // One question, one answer: the writers obey `refuse_write`, so the
        // health report must too. A workspace this build refuses to write into
        // is not one it may call `ready` — and the refusal's own wording is the
        // most useful diagnosis available.
        if let Err(refusal) = self.refuse_write() {
            return Ok(HealthReport {
                status: WorkspaceStatus::Invalid,
                schema_version,
                sqlite: None,
                message: Some(refusal.to_string()),
            });
        }

        let (status, message) = match schema_version {
            Some(SchemaVersion(CURRENT_SCHEMA_VERSION)) => (WorkspaceStatus::Ready, None),
            Some(SchemaVersion(found)) if found > CURRENT_SCHEMA_VERSION => (
                WorkspaceStatus::Invalid,
                Some(format!(
                    "database schema version {found} is newer than supported {CURRENT_SCHEMA_VERSION}"
                )),
            ),
            Some(SchemaVersion(found)) => (
                WorkspaceStatus::Invalid,
                Some(format!(
                    "database schema version {found} requires migrations"
                )),
            ),
            None => (
                WorkspaceStatus::Uninitialized,
                Some("database has not been initialized".to_owned()),
            ),
        };

        Ok(HealthReport {
            status,
            schema_version,
            sqlite: None,
            message,
        })
    }

    /// Returns a small local stats projection.
    ///
    /// Status and version come from [`Store::health`] — one derivation, so the
    /// two answers cannot drift — and the counts are read only when the
    /// canonical tables are known to exist.
    pub fn stats(&self) -> Result<StatsReport, StoreError> {
        let health = self.health()?;
        let initialized = health.status == WorkspaceStatus::Ready;

        let (sources, chunks) = if initialized {
            let counts = self.counts()?;
            (Some(counts.sources), Some(counts.chunks))
        } else {
            (None, None)
        };

        Ok(StatsReport {
            status: health.status,
            schema_version: health.schema_version,
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
    fn the_linked_engine_is_out_of_the_wal_reset_window() {
        // An instrument that checks itself: the engine version is a build fact,
        // and a regression here (a downgraded rusqlite) must fail the suite
        // rather than ship a database writer with known corruption bugs.
        let facts = sqlite_facts().expect("read engine facts");

        let expected: i32 = facts
            .version
            .split('.')
            .enumerate()
            .map(|(index, part)| {
                let weight = [1_000_000, 1_000, 1][index];
                part.parse::<i32>().expect("numeric version part") * weight
            })
            .sum();
        assert_eq!(
            facts.version_number, expected,
            "SQL and the crate agree on the version ({})",
            facts.version
        );
        assert!(
            !facts.wal_reset_window,
            "bundled SQLite {} is inside the WAL-reset window",
            facts.version
        );
        assert!(
            !facts.withdrawn_release,
            "bundled SQLite {} is a withdrawn release",
            facts.version
        );
        assert!(!facts.source_id.is_empty(), "source id is reported");
        assert!(
            !facts.compile_options.is_empty(),
            "compile options are reported"
        );
    }

    #[test]
    fn every_refusal_shape_leaves_the_file_byte_identical() {
        // One snapshot, every shape: a new refusal path may not be covered for
        // only some of the observable facts. Raw bytes make even a header-level
        // write (a journal-mode switch) visible.
        let shapes: [(&str, String); 11] = [
            (
                "foreign identity",
                "PRAGMA application_id = 305419896; CREATE TABLE t(x);".to_owned(),
            ),
            (
                "foreign tables",
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);".to_owned(),
            ),
            ("future version", "PRAGMA user_version = 99;".to_owned()),
            (
                "our identity, future version",
                format!("PRAGMA application_id = {APPLICATION_ID}; PRAGMA user_version = 99;"),
            ),
            (
                "a table that merely shares our name",
                "CREATE TABLE chunks (id INTEGER PRIMARY KEY);".to_owned(),
            ),
            (
                "legacy carrier",
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta (key, value) VALUES ('schema_version', '2');"
                    .to_owned(),
            ),
            (
                "our identity, legacy carrier",
                format!(
                    "PRAGMA application_id = {APPLICATION_ID};
                     CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     INSERT INTO schema_meta (key, value) VALUES ('schema_version', '2');"
                ),
            ),
            (
                "version carrier, no identity",
                "PRAGMA user_version = 3;".to_owned(),
            ),
            (
                "version carrier, no identity, foreign objects",
                "PRAGMA user_version = 3; CREATE TABLE notes (id INTEGER PRIMARY KEY);".to_owned(),
            ),
            // v5 names in a file that is not ours: the new tables are no more
            // adoptable than `chunks` was.
            (
                "v5 table names, no identity",
                "CREATE TABLE answers (answer_id TEXT PRIMARY KEY);
                 CREATE TABLE graph_links (from_source_key TEXT NOT NULL);"
                    .to_owned(),
            ),
            (
                "version carrier, no identity, v5 table names",
                "PRAGMA user_version = 5;
                 CREATE TABLE graph_links (from_source_key TEXT NOT NULL);"
                    .to_owned(),
            ),
        ];

        for (name, sql) in shapes {
            let dir = tempfile::tempdir().expect("create temp dir");
            let path = dir.path().join("momotaro.db");
            {
                let connection = rusqlite::Connection::open(&path).expect("open raw");
                connection.execute_batch(&sql).expect("seed the shape");
            }

            let before = db_facts(&path);
            let mut store = Store::open(&path).expect("open store");
            let error = store
                .init_schema()
                .expect_err(&format!("{name} must be refused"));

            assert_eq!(
                before,
                db_facts(&path),
                "{name} was refused with {error} but the file changed"
            );
        }
    }

    /// Every observable fact about a database file that a refusal must leave
    /// untouched. Read-only on purpose: opening read-write can checkpoint a WAL
    /// and change the very bytes under test.
    #[derive(Debug, PartialEq)]
    struct DbFacts {
        objects: Vec<String>,
        journal_mode: String,
        application_id: i32,
        user_version: i64,
        schema_meta: Vec<(String, String)>,
        bytes: Vec<u8>,
    }

    fn db_facts(path: &Path) -> DbFacts {
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open raw");
        let objects: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY name")
            .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
            .expect("list objects");
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        let application_id: i32 = connection
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .expect("read application_id");
        let user_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        let schema_meta: Vec<(String, String)> = if objects.iter().any(|name| name == SCHEMA_TABLE)
        {
            connection
                .prepare(&format!(
                    "SELECT key, value FROM {SCHEMA_TABLE} ORDER BY key"
                ))
                .and_then(|mut statement| {
                    statement
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect()
                })
                .expect("read schema_meta")
        } else {
            Vec::new()
        };
        drop(connection);

        DbFacts {
            objects,
            journal_mode,
            application_id,
            user_version,
            schema_meta,
            bytes: std::fs::read(path).expect("read file bytes"),
        }
    }

    #[test]
    fn future_schema_is_rejected() {
        let (_dir, mut store) = open_temp_store();
        store
            .connection
            .execute_batch(
                "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 PRAGMA user_version = 999;",
            )
            .expect("write future schema");

        let error = store.init_schema().expect_err("future schema must fail");
        assert!(matches!(error, StoreError::SchemaTooNew { .. }));
    }

    #[test]
    fn fresh_db_records_user_version_and_application_id() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("init");

        assert_eq!(
            store.user_version().expect("read user_version"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(store.application_id().expect("read app id"), APPLICATION_ID);
        // schema_meta is left for multi-value metadata (D46 / Q2).
        assert_eq!(
            store
                .metadata("key_scheme")
                .expect("read key_scheme")
                .as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn foreign_database_is_rejected_without_touching_it() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("other.db");
        {
            let connection = rusqlite::Connection::open(&path).expect("open foreign db");
            connection
                .execute_batch("PRAGMA application_id = 305419896; CREATE TABLE t(x);")
                .expect("seed foreign db");
        }

        let mut store = Store::open(&path).expect("open store");
        let error = store.init_schema().expect_err("foreign db must fail");
        assert!(matches!(error, StoreError::ForeignDatabase { .. }));
        // The write-free property for every refusal shape, including this one,
        // is pinned by `every_refusal_shape_leaves_the_file_byte_identical`.
    }

    #[test]
    fn a_file_with_foreign_objects_and_no_identity_is_not_adopted() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("notes.db");
        {
            // Somebody else's plain SQLite database: no application id, no
            // version carrier, and none of our tables.
            let connection = rusqlite::Connection::open(&path).expect("open plain db");
            connection
                .execute_batch(
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
                     INSERT INTO notes (body) VALUES ('not ours');",
                )
                .expect("seed plain db");
        }

        let mut store = Store::open(&path).expect("open store");
        let error = store
            .init_schema()
            .expect_err("a foreign object set must not be adopted");
        assert!(
            matches!(&error, StoreError::ForeignObjects { objects } if objects == "notes"),
            "the offending objects are named in the error itself: {error}"
        );
        // Write-freeness (objects, journal mode, identity, bytes) is pinned by
        // `every_refusal_shape_leaves_the_file_byte_identical`.
    }

    #[test]
    fn a_store_that_needs_no_migration_is_not_backed_up() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");

        let mut store = Store::open(&path).expect("open store");
        store.init_schema().expect("first init");
        assert!(
            backup_files(dir.path(), "backup-").is_empty(),
            "a brand-new store has nothing to copy"
        );
        drop(store);

        let mut store = Store::open(&path).expect("reopen store");
        store.init_schema().expect("second init");
        assert!(
            backup_files(dir.path(), "backup-").is_empty(),
            "an up-to-date store is not copied on every open"
        );
    }

    /// Seeds a faithful v2 store: the exact v2 DDL of migration step 2 (both
    /// tables and both indexes), the file identity, and the version carrier —
    /// but no `raw_name` column.
    ///
    /// Winding a v3 store's version back is NOT the same thing (the v3 DDL
    /// would then re-run and fail, correctly). Stores written by this repo's
    /// *old* builds recorded the version in `schema_meta` and are refused as
    /// `LegacyStore`; this shape is what a future v2 → v3 migration will see.
    fn seed_v2_store(path: &Path) {
        let connection = rusqlite::Connection::open(path).expect("open v2 store");
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID};
                 CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE source_revisions (
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
                 CREATE INDEX idx_revisions_current
                   ON source_revisions(source_key) WHERE is_current = 1;
                 INSERT INTO schema_meta (key, value) VALUES ('key_scheme', 'v2');
                 PRAGMA user_version = 2;"
            ))
            .expect("seed a v2 store");
    }

    #[test]
    fn a_blocked_backup_stops_the_migration_before_any_ddl() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        seed_v2_store(&path);

        // Block every plausible temp-artifact name with a *directory*: the
        // pre-cleanup then fails, so `VACUUM INTO` never runs and the migration
        // must refuse to start (distribution-trust §8.2). This pins the `?`
        // wiring in `init_schema` specifically — a mutation that ignored the
        // backup error left every other test green.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("read the clock")
            .as_secs();
        for offset in 0..30 {
            let blocker = dir.path().join(format!(
                ".tmp-backup-backup-v2-to-v{CURRENT_SCHEMA_VERSION}-{}.db",
                now + offset
            ));
            std::fs::create_dir(&blocker).expect("block the temp artifact path");
        }

        let mut store = Store::open(&path).expect("reopen store");
        let error = store
            .init_schema()
            .expect_err("a blocked backup must stop the migration");
        assert!(matches!(error, StoreError::BackupFailed(_)), "{error}");
        assert_eq!(
            store.user_version().expect("read user_version"),
            2,
            "the migration never started"
        );
        assert!(
            !column_exists(&store, "source_revisions", "raw_name"),
            "and none of the v3 DDL ran"
        );
    }

    /// Whether `table` has `column` — reads the schema, never writes.
    fn column_exists(store: &Store, table: &str, column: &str) -> bool {
        store
            .connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .expect("read table_info")
            .iter()
            .any(|name| name == column)
    }

    /// File names in `dir` that are published backups with `prefix`.
    fn backup_files(dir: &Path, prefix: &str) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read workspace dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    #[test]
    fn pending_migration_writes_a_verified_backup_first() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        seed_v2_store(&path);

        let mut store = Store::open(&path).expect("reopen store");
        store.init_schema().expect("migrate with backup");

        let backups = backup_files(
            dir.path(),
            &format!("backup-v2-to-v{CURRENT_SCHEMA_VERSION}-"),
        );
        assert_eq!(backups.len(), 1, "one verified backup before migrating");
        assert_eq!(
            store.user_version().expect("read user_version"),
            CURRENT_SCHEMA_VERSION
        );
        let artifact = dir.path().join(&backups[0]);
        backup::verify_artifact(&artifact).expect("the published backup verifies");

        // The migration really landed: the column exists afterwards.
        let has_raw_name = {
            let mut statement = store
                .connection
                .prepare("PRAGMA table_info(source_revisions)")
                .expect("prepare table_info");
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .expect("query table_info")
                .filter_map(Result::ok)
                .any(|column| column == "raw_name")
        };
        assert!(has_raw_name, "v3 adds the raw_name column");
    }

    #[test]
    fn a_migration_failure_leaves_the_key_scheme_unstamped() {
        // The documented order puts the key-scheme stamp AFTER the migrations,
        // so a migration that fails must not leave the stamp behind. The seed
        // carries `raw_name` already (the v3 ALTER then fails) and no
        // `key_scheme` row — stamping first would answer `Some("v2")` here.
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        {
            let connection = rusqlite::Connection::open(&path).expect("open seeded store");
            connection
                .execute_batch(&format!(
                    "PRAGMA application_id = {APPLICATION_ID};
                     CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     CREATE TABLE source_revisions (
                       source_key TEXT NOT NULL,
                       revision_hash TEXT NOT NULL,
                       kind TEXT NOT NULL,
                       title TEXT,
                       uri TEXT,
                       local_path TEXT,
                       raw_name TEXT,
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
                     PRAGMA user_version = 2;"
                ))
                .expect("seed a store whose migration will fail");
        }

        let mut store = Store::open(&path).expect("open store");
        store
            .init_schema()
            .expect_err("the v3 ALTER fails on the seeded raw_name");
        assert_eq!(
            store.metadata("key_scheme").expect("read key_scheme"),
            None,
            "a failed migration stamps no key scheme"
        );
    }

    /// Seeds a faithful v4 store: the v2 tables and indexes, the v3 `raw_name`
    /// ALTER (exactly as a migrated store carries it, last in `table_info`),
    /// the v4 `store_events` table with its two partial indexes, the file
    /// identity, and the version carrier. The rows in `source_revisions` /
    /// `chunks` / `store_events` are sentinels: the v5 migration only adds
    /// tables and must not touch these — not their rows, not their DDL.
    fn seed_v4_store(path: &Path) {
        let connection = rusqlite::Connection::open(path).expect("open v4 store");
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID};
                 CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE source_revisions (
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
                 CREATE INDEX idx_revisions_current
                   ON source_revisions(source_key) WHERE is_current = 1;
                 ALTER TABLE source_revisions ADD COLUMN raw_name TEXT;
                 CREATE TABLE store_events (
                   id INTEGER PRIMARY KEY,
                   occurred_at INTEGER NOT NULL,
                   recorded_at INTEGER NOT NULL,
                   batch_id TEXT NOT NULL,
                   seq INTEGER NOT NULL,
                   domain TEXT NOT NULL,
                   type TEXT NOT NULL,
                   source_key TEXT,
                   outcome TEXT,
                   reason_code TEXT,
                   message TEXT,
                   data_json TEXT NOT NULL,
                   external_id TEXT,
                   UNIQUE (batch_id, seq)
                 );
                 CREATE UNIQUE INDEX idx_store_events_external
                   ON store_events(domain, external_id) WHERE external_id IS NOT NULL;
                 CREATE INDEX idx_store_events_source
                   ON store_events(source_key) WHERE source_key IS NOT NULL;
                 INSERT INTO schema_meta (key, value) VALUES ('key_scheme', 'v2');
                 INSERT INTO source_revisions
                   (source_key, revision_hash, kind, title, uri, local_path, origin_class,
                    metadata_json, ingested_at, is_current, raw_name)
                   VALUES ('note:keep.md', 'k1', 'note', 'Keep', NULL, 'keep.md', 'owner',
                    '{{}}', 1, 1, 'keep.md');
                 INSERT INTO chunks
                   (chunk_id, source_key, revision_hash, ordinal, heading_path, text,
                    locator_json, chunk_hash, token_estimate)
                   VALUES ('ck1', 'note:keep.md', 'k1', 0, NULL, 'keep me',
                    '{{}}', 'hk1', 1);
                 INSERT INTO store_events
                   (occurred_at, recorded_at, batch_id, seq, domain, type, source_key,
                    outcome, reason_code, message, data_json, external_id)
                   VALUES (1, 1, 'b1', 1, 'ingest', 'ingest.started', NULL,
                    NULL, NULL, NULL, '{{}}', NULL);
                 PRAGMA user_version = 4;"
            ))
            .expect("seed a v4 store");
    }

    /// One column as `PRAGMA table_info` reports it: name, declared type,
    /// NOT NULL flag, default expression, primary-key position (`0` = not in
    /// the key). The whole tuple is asserted, so a column cannot quietly lose
    /// a constraint.
    type ColumnShape = (&'static str, &'static str, bool, Option<&'static str>, i32);
    type OwnedColumnShape = (String, String, bool, Option<String>, i32);
    type TableShape = (String, Vec<OwnedColumnShape>, Vec<String>);

    fn column_shape(store: &Store, table: &str) -> Vec<OwnedColumnShape> {
        store
            .connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, bool>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, i32>(5)?,
                        ))
                    })?
                    .collect()
            })
            .expect("read table_info")
    }

    /// The v5 tables exactly as architecture §8.1 / §8.2 and multi-agent §5.1
    /// declare them. One list per table, columns in declaration order — a
    /// column missing from the migration step turns its assertion red.
    const V5_SHAPES: &[(&str, &[ColumnShape])] = &[
        (
            "sessions",
            &[
                ("session_id", "TEXT", false, None, 1),
                ("parent_session_id", "TEXT", false, None, 0),
                ("project_id", "TEXT", false, None, 0),
                ("title", "TEXT", true, None, 0),
                ("title_source", "TEXT", true, Some("'derived'"), 0),
                ("profile_id", "TEXT", false, None, 0),
                ("first_user_message", "TEXT", false, None, 0),
                ("archived", "INTEGER", true, Some("0"), 0),
                ("archived_at", "INTEGER", false, None, 0),
                ("pinned", "INTEGER", true, Some("0"), 0),
                ("created_at", "INTEGER", true, None, 0),
                ("updated_at", "INTEGER", true, None, 0),
                ("last_activity_at", "INTEGER", false, None, 0),
            ],
        ),
        (
            "runs",
            &[
                ("run_id", "TEXT", false, None, 1),
                ("session_id", "TEXT", false, None, 0),
                ("parent_run_id", "TEXT", false, None, 0),
                ("project_id", "TEXT", false, None, 0),
                ("graph_id", "TEXT", false, None, 0),
                ("node_id", "TEXT", false, None, 0),
                ("attempt", "INTEGER", true, Some("1"), 0),
                ("mode", "TEXT", true, None, 0),
                ("query", "TEXT", true, None, 0),
                ("status", "TEXT", true, None, 0),
                ("client_request_id", "TEXT", false, None, 0),
                ("run_spec_hash", "TEXT", true, None, 0),
                ("run_spec_json", "TEXT", true, None, 0),
                ("provider", "TEXT", false, None, 0),
                ("model", "TEXT", false, None, 0),
                ("context_window", "INTEGER", false, None, 0),
                ("max_output_tokens", "INTEGER", false, None, 0),
                ("result_digest", "TEXT", false, None, 0),
                ("failure_note", "TEXT", false, None, 0),
                ("deadline_at", "INTEGER", false, None, 0),
                ("created_at", "INTEGER", true, None, 0),
                ("finished_at", "INTEGER", false, None, 0),
            ],
        ),
        (
            "run_events",
            &[
                ("run_id", "TEXT", true, None, 1),
                ("seq", "INTEGER", true, None, 2),
                ("type", "TEXT", true, None, 0),
                ("data_json", "TEXT", true, None, 0),
                ("created_at", "INTEGER", true, None, 0),
            ],
        ),
        (
            "history_items",
            &[
                ("history_id", "TEXT", false, None, 1),
                ("session_id", "TEXT", false, None, 0),
                ("run_id", "TEXT", true, None, 0),
                ("seq", "INTEGER", true, None, 0),
                ("kind", "TEXT", true, None, 0),
                ("role", "TEXT", true, None, 0),
                ("content_kind", "TEXT", true, None, 0),
                ("body", "TEXT", true, None, 0),
                ("payload_json", "TEXT", true, None, 0),
                ("source_ref_json", "TEXT", false, None, 0),
                ("token_estimate", "INTEGER", true, None, 0),
                ("created_at", "INTEGER", true, None, 0),
                ("occurred_at", "INTEGER", false, None, 0),
            ],
        ),
        (
            "retrievals",
            &[
                ("retrieval_id", "TEXT", false, None, 1),
                ("run_id", "TEXT", true, None, 0),
                ("query", "TEXT", true, None, 0),
                ("normalized_query", "TEXT", true, None, 0),
                ("strategy_json", "TEXT", true, None, 0),
                ("created_at", "INTEGER", true, None, 0),
            ],
        ),
        (
            "retrieval_hits",
            &[
                ("retrieval_id", "TEXT", true, None, 1),
                ("rank", "INTEGER", true, None, 0),
                ("source_key", "TEXT", true, None, 0),
                ("revision_hash", "TEXT", true, None, 0),
                ("chunk_id", "TEXT", true, None, 2),
                ("chunk_hash", "TEXT", true, None, 0),
                ("locator_json", "TEXT", true, None, 0),
                ("score", "REAL", false, None, 0),
                ("included", "INTEGER", true, None, 0),
                ("omitted_reason", "TEXT", false, None, 0),
            ],
        ),
        (
            "run_context_items",
            &[
                ("run_id", "TEXT", true, None, 1),
                ("ordinal", "INTEGER", true, None, 2),
                ("fragment_id", "TEXT", true, None, 0),
                ("kind", "TEXT", true, None, 0),
                ("role", "TEXT", true, None, 0),
                ("content_kind", "TEXT", true, None, 0),
                ("body", "TEXT", true, None, 0),
                ("source_ref_json", "TEXT", false, None, 0),
                ("token_estimate", "INTEGER", true, None, 0),
                ("body_hash", "TEXT", true, None, 0),
                ("decision_rationale", "TEXT", false, None, 0),
                ("assumptions", "TEXT", false, None, 0),
                ("supersedes", "TEXT", false, None, 0),
            ],
        ),
        (
            "llm_turns",
            &[
                ("turn_id", "TEXT", false, None, 1),
                ("run_id", "TEXT", true, None, 0),
                ("seq", "INTEGER", true, None, 0),
                ("provider", "TEXT", true, None, 0),
                ("model", "TEXT", true, None, 0),
                ("request_hash", "TEXT", true, None, 0),
                ("status", "TEXT", true, None, 0),
                ("input_tokens", "INTEGER", false, None, 0),
                ("output_tokens", "INTEGER", false, None, 0),
                ("cached_tokens", "INTEGER", false, None, 0),
                ("billable_tokens", "INTEGER", false, None, 0),
                ("slot", "TEXT", false, None, 0),
                ("reasoning_tokens", "INTEGER", false, None, 0),
                ("raw_ref", "TEXT", false, None, 0),
                ("created_at", "INTEGER", true, None, 0),
                ("finished_at", "INTEGER", false, None, 0),
            ],
        ),
        (
            "answers",
            &[
                ("answer_id", "TEXT", false, None, 1),
                ("run_id", "TEXT", true, None, 0),
                ("text", "TEXT", true, None, 0),
                ("prompt_hash", "TEXT", true, None, 0),
                ("created_at", "INTEGER", true, None, 0),
                ("decision_rationale", "TEXT", false, None, 0),
                ("assumptions", "TEXT", false, None, 0),
                ("supersedes", "TEXT", false, None, 0),
            ],
        ),
        (
            "answer_citations",
            &[
                ("answer_id", "TEXT", true, None, 1),
                ("citation_index", "INTEGER", true, None, 2),
                ("source_key", "TEXT", true, None, 0),
                ("revision_hash", "TEXT", true, None, 0),
                ("chunk_id", "TEXT", true, None, 0),
                ("chunk_hash", "TEXT", true, None, 0),
                ("locator_json", "TEXT", true, None, 0),
                ("quoted_text", "TEXT", true, None, 0),
                ("verified", "INTEGER", true, None, 0),
            ],
        ),
        (
            "verifications",
            &[
                ("verification_id", "TEXT", false, None, 1),
                ("run_id", "TEXT", true, None, 0),
                ("answer_id", "TEXT", false, None, 0),
                ("kind", "TEXT", true, None, 0),
                ("status", "TEXT", true, None, 0),
                ("details_json", "TEXT", true, None, 0),
                ("created_at", "INTEGER", true, None, 0),
            ],
        ),
        (
            "graph_links",
            &[
                ("from_source_key", "TEXT", true, None, 0),
                ("to_source_key", "TEXT", false, None, 0),
                ("raw_target", "TEXT", true, None, 0),
                ("edge_kind", "TEXT", true, None, 0),
                ("link_source", "TEXT", true, None, 0),
                ("anchor_kind", "TEXT", false, None, 0),
                ("anchor_raw", "TEXT", false, None, 0),
                ("created_at", "INTEGER", true, None, 0),
            ],
        ),
    ];

    #[test]
    fn the_v5_tables_have_their_contract_columns() {
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("initialize schema");

        for (table, expected) in V5_SHAPES {
            let expected: Vec<OwnedColumnShape> = expected
                .iter()
                .map(|(name, ty, notnull, dflt, pk)| {
                    (
                        (*name).to_owned(),
                        (*ty).to_owned(),
                        *notnull,
                        (*dflt).map(str::to_owned),
                        *pk,
                    )
                })
                .collect();
            assert_eq!(
                column_shape(&store, table),
                expected,
                "{table} must have exactly its contract columns"
            );
        }

        // UNIQUE declarations live in the CREATE text and carry the
        // autoindexes the access paths rely on — pin the two of them.
        for (table, needle) in [
            ("history_items", "UNIQUE (run_id, seq)"),
            ("runs", "client_request_id TEXT UNIQUE"),
        ] {
            let sql: String = store
                .connection
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("read create text");
            assert!(sql.contains(needle), "{table} must declare {needle}: {sql}");
        }
    }

    /// Every non-bookkeeping table's columns and index names — the comparable
    /// skeleton of a schema. Create-text whitespace differs between a seeded
    /// and a migrated store, so shapes are compared, not SQL text.
    fn schema_shape(store: &Store) -> Vec<TableShape> {
        let tables: Vec<String> = store
            .connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
            .expect("list tables");
        tables
            .into_iter()
            .map(|table| {
                let indexes: Vec<String> = store
                    .connection
                    .prepare(
                        "SELECT name FROM sqlite_master
                         WHERE type = 'index' AND tbl_name = ?1 ORDER BY name",
                    )
                    .and_then(|mut statement| {
                        statement
                            .query_map([&table], |row| row.get::<_, String>(0))?
                            .collect()
                    })
                    .expect("list indexes");
                let columns = column_shape(store, &table);
                (table, columns, indexes)
            })
            .collect()
    }

    /// Every row of `table`, values rendered for exact comparison.
    fn table_rows(connection: &rusqlite::Connection, table: &str) -> Vec<Vec<String>> {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .expect("select rows");
        let columns = statement.column_count();
        let mut rows = statement.query([]).expect("query rows");
        let mut out = Vec::new();
        while let Some(row) = rows.next().expect("next row") {
            let mut values = Vec::with_capacity(columns);
            for index in 0..columns {
                let value: rusqlite::types::Value = row.get(index).expect("read value");
                values.push(format!("{value:?}"));
            }
            out.push(values);
        }
        out
    }

    /// DDL text and full row content of the tables v5 must not rebuild.
    fn existing_tables_facts(path: &Path) -> Vec<(String, String, Vec<Vec<String>>)> {
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open raw");
        ["source_revisions", "chunks", "store_events"]
            .into_iter()
            .map(|table| {
                let sql: String = connection
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        [table],
                        |row| row.get(0),
                    )
                    .expect("read create text");
                let rows = table_rows(&connection, table);
                (table.to_owned(), sql, rows)
            })
            .collect()
    }

    #[test]
    fn a_v4_store_migrates_to_v5_without_touching_the_existing_tables() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        seed_v4_store(&path);
        let before = existing_tables_facts(&path);

        let mut store = Store::open(&path).expect("reopen store");
        store.init_schema().expect("migrate to current");

        assert_eq!(
            store.user_version().expect("read user_version"),
            CURRENT_SCHEMA_VERSION
        );
        let backups = backup_files(
            dir.path(),
            &format!("backup-v4-to-v{CURRENT_SCHEMA_VERSION}-"),
        );
        assert_eq!(backups.len(), 1, "one verified backup before migrating");

        drop(store);
        assert_eq!(
            existing_tables_facts(&path),
            before,
            "the v3/v4 tables keep their DDL and their rows"
        );
    }

    #[test]
    fn a_migrated_v4_store_and_a_fresh_store_land_on_the_same_shape() {
        // The two supported paths into the current schema — migrating a v4
        // store and initializing a brand-new file — must arrive at the same
        // tables, columns and indexes.
        let fresh_dir = tempfile::tempdir().expect("create temp dir");
        let fresh_path = fresh_dir.path().join("momotaro.db");
        let mut fresh = Store::open(&fresh_path).expect("open fresh store");
        fresh.init_schema().expect("initialize fresh");

        let migrated_dir = tempfile::tempdir().expect("create temp dir");
        let migrated_path = migrated_dir.path().join("momotaro.db");
        seed_v4_store(&migrated_path);
        let mut migrated = Store::open(&migrated_path).expect("open v4 store");
        migrated.init_schema().expect("migrate to current");

        assert_eq!(
            schema_shape(&fresh),
            schema_shape(&migrated),
            "both paths must land on the same schema"
        );
    }

    #[test]
    fn a_legacy_store_is_refused_with_a_clear_message() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("momotaro.db");
        {
            // What this repo's own pre-v3 builds wrote: the full v2 shape, the
            // version in `schema_meta`, and no application id at all. Such a
            // store cannot be migrated — its rows carry pre-scheme keys — so it
            // is refused instead of half-adopted.
            let connection = rusqlite::Connection::open(&path).expect("open legacy store");
            connection
                .execute_batch(
                    "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                     CREATE TABLE source_revisions (
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
                     CREATE INDEX idx_revisions_current
                       ON source_revisions(source_key) WHERE is_current = 1;
                     INSERT INTO schema_meta (key, value) VALUES ('schema_version', '2');",
                )
                .expect("seed legacy store");
        }

        let mut store = Store::open(&path).expect("open store");
        let error = store
            .init_schema()
            .expect_err("a pre-user_version store must be refused");
        assert!(
            matches!(&error, StoreError::LegacyStore { found } if found == "2"),
            "the legacy version is a structured field: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("delete the workspace data directory"),
            "the message must be actionable: {error}"
        );
        // Byte-level write-freeness is pinned by
        // `every_refusal_shape_leaves_the_file_byte_identical`.
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
    fn fresh_db_creates_the_revision_and_chunk_tables_and_indexes() {
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
        // A v1 store of ours: the identity stamp comes with the version carrier
        // (same change), so this is what our own older builds actually left.
        store
            .connection
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID};
                 CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO schema_meta (key, value) VALUES ('sentinel', 'keep-me');
                 PRAGMA user_version = 1;"
            ))
            .expect("create v1 baseline");

        store.init_schema().expect("migrate to current");

        assert_eq!(
            store.schema_version().expect("read schema"),
            Some(SchemaVersion::CURRENT)
        );
        assert_eq!(
            store.user_version().expect("read user_version"),
            CURRENT_SCHEMA_VERSION
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
    fn health_and_stats_never_call_a_file_ready_that_the_writers_refuse() {
        // The refusal predicate is one question asked in both directions. Before
        // this, a file with a version carrier and no identity was reported
        // `ready` (exit 0) while `init`, `index` and `rebuild-index` all refused
        // it, which breaks the documented `doctor && index` gate.
        let (_dir, store) = open_temp_store();
        store
            .connection
            .execute_batch(
                "PRAGMA user_version = 3;
                 CREATE TABLE notes (id INTEGER PRIMARY KEY);",
            )
            .expect("seed a file that only looks like ours");

        let health = store.health().expect("health");
        assert_eq!(health.status, WorkspaceStatus::Invalid);
        let message = health.message.expect("a refusal explains itself");
        assert!(
            message.contains("identity"),
            "the reason, not a shrug: {message}"
        );

        let stats = store.stats().expect("stats");
        assert_eq!(stats.status, WorkspaceStatus::Invalid);
        assert!(!stats.initialized);
        assert_eq!((stats.sources, stats.chunks), (None, None));
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
                 PRAGMA user_version = 999;",
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
                 PRAGMA user_version = 1;",
            )
            .expect("write v1 schema");

        let report = store.stats().expect("stats");
        assert_eq!(report.status, WorkspaceStatus::Invalid);
        assert!(!report.initialized);
        assert_eq!(report.schema_version, Some(SchemaVersion(1)));
    }

    #[test]
    fn current_origins_returns_the_class_of_each_current_source() {
        // The rebuild path looks up one class per source key, and a wrong or
        // missing entry silently changes what a source *is* (T1.8 weights read
        // this), so the query is pinned here as well as through the index.
        let (_dir, mut store) = open_temp_store();
        store.init_schema().expect("schema");
        for (key, hash, class) in [
            ("note:paper.md", "p1", OriginClass::Paper),
            ("note:mine.md", "m1", OriginClass::Owner),
        ] {
            store
                .append_revision(&NewSourceRevision {
                    source_key: key,
                    revision_hash: hash,
                    kind: SourceKind::Note,
                    title: None,
                    uri: None,
                    local_path: None,
                    raw_name: None,
                    origin_class: class,
                    metadata_json: "{}",
                    ingested_at: 1,
                })
                .expect("append");
        }

        let keys: std::collections::HashSet<&str> =
            ["note:paper.md", "note:mine.md", "note:absent.md"]
                .into_iter()
                .collect();
        let origins = store.current_origins(&keys).expect("origins");

        assert_eq!(origins.get("note:paper.md"), Some(&OriginClass::Paper));
        assert_eq!(origins.get("note:mine.md"), Some(&OriginClass::Owner));
        assert!(
            !origins.contains_key("note:absent.md"),
            "a source with no current revision is absent, never defaulted"
        );
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
                raw_name: None,
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
                raw_name: None,
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
            .replace_chunks("a.md", "a2", &chunks, &[])
            .expect("replace chunks");

        let report = store.stats().expect("stats");
        assert!(report.initialized);
        assert_eq!(report.sources, Some(1));
        assert_eq!(report.chunks, Some(2));
    }

    #[test]
    fn the_schema_declares_exactly_the_expected_indexes() {
        // The one explicit index list, pinned: sqlite_autoindex_* rows come
        // with their constraints and are excluded (`sql` is NULL for them), so
        // this asserts what the schema *declares* — a new or dropped explicit
        // index is a schema change this test must see (the step-5 assertion the
        // v5 batch deferred to W2.4).
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open");
        store.init_schema().expect("init");
        let mut names = Vec::new();
        {
            let mut statement = store
                .connection
                .prepare(
                    "SELECT name FROM sqlite_master
                     WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
                )
                .expect("prepare");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            for row in rows {
                names.push(row.expect("row"));
            }
        }
        assert_eq!(
            names,
            [
                "idx_chunks_rev",
                "idx_graph_links_from",
                "idx_revisions_current",
                "idx_store_events_external",
                "idx_store_events_source",
            ]
        );
    }
}
