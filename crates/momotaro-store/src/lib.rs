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
mod migration;
pub mod revisions;

pub use chunks::SourceCounts;
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
    /// Order is the contract (D46): identity check → version read → verified
    /// backup of an existing store → migrations → key-scheme stamp.
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
        let shapes: [(&str, String); 9] = [
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
            let blocker = dir
                .path()
                .join(format!(".tmp-backup-backup-v2-to-v3-{}.db", now + offset));
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

        let backups = backup_files(dir.path(), "backup-v2-to-v3-");
        assert_eq!(backups.len(), 1, "one verified backup before migrating");
        assert_eq!(
            store.user_version().expect("read user_version"),
            CURRENT_SCHEMA_VERSION
        );

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
            .replace_chunks("a.md", "a2", &chunks)
            .expect("replace chunks");

        let report = store.stats().expect("stats");
        assert!(report.initialized);
        assert_eq!(report.sources, Some(1));
        assert_eq!(report.chunks, Some(2));
    }
}
