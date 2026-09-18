//! Reading and appending canonical source revisions.

use std::collections::{HashMap, HashSet};

use momotaro_contracts::{OriginClass, SourceKind, SourceRevision};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{StoreError, invalid_value};

/// `?1, ?2, …` covering `keys`, for an `IN` clause bound positionally.
fn numbered_placeholders(keys: &HashSet<&str>) -> String {
    (1..=keys.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A new source revision to append; the store owns the `is_current` flag.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSourceRevision<'a> {
    /// Canonical identity, e.g. vault-relative path or `arxiv:NNNN.NNNNN`.
    pub source_key: &'a str,
    /// `sha256` over normalized source bytes.
    pub revision_hash: &'a str,
    /// Content kind.
    pub kind: SourceKind,
    /// Display title, when derivable.
    pub title: Option<&'a str>,
    /// Remote URI, when the source has one.
    pub uri: Option<&'a str>,
    /// Workspace-relative, `/`-joined path when the source lives on disk
    /// (ADR 0024). Never absolute: the store only describes what lives inside
    /// the workspace that holds it.
    pub local_path: Option<&'a str>,
    /// The file name exactly as the vault spelled it (display / rename
    /// matching). Never an identity: `source_key` is.
    pub raw_name: Option<&'a str>,
    /// Trust origin.
    pub origin_class: OriginClass,
    /// JSON metadata blob (fingerprint, tool tag, provider data).
    pub metadata_json: &'a str,
    /// Unix epoch seconds at ingestion.
    pub ingested_at: i64,
}

/// Decodes the stored spelling, which lives on [`SourceKind`] (`as_str` / `FromStr`).
fn kind_from_string(value: &str) -> Result<SourceKind, StoreError> {
    value.parse().map_err(|_| invalid_value("kind", value))
}

pub(crate) fn origin_from_string(value: &str) -> Result<OriginClass, StoreError> {
    value
        .parse()
        .map_err(|_| invalid_value("origin_class", value))
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRevision> {
    let kind: String = row.get("kind")?;
    let origin_class: String = row.get("origin_class")?;

    let kind = match kind_from_string(&kind) {
        Ok(kind) => kind,
        Err(error) => return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(error))),
    };
    let origin_class = match origin_from_string(&origin_class) {
        Ok(origin) => origin,
        Err(error) => return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(error))),
    };

    Ok(SourceRevision {
        source_key: row.get("source_key")?,
        revision_hash: row.get("revision_hash")?,
        kind,
        title: row.get("title")?,
        uri: row.get("uri")?,
        local_path: row.get("local_path")?,
        raw_name: row.get("raw_name")?,
        origin_class,
        metadata_json: row.get("metadata_json")?,
        ingested_at: row.get("ingested_at")?,
        is_current: row.get::<_, i64>("is_current")? != 0,
    })
}

fn query_current(
    connection: &Connection,
    source_key: &str,
) -> Result<Option<SourceRevision>, StoreError> {
    let revision = connection
        .query_row(
            "SELECT source_key, revision_hash, kind, title, uri, local_path, raw_name,
                    origin_class, metadata_json, ingested_at, is_current
             FROM source_revisions
             WHERE source_key = ?1 AND is_current = 1",
            params![source_key],
            read_row,
        )
        .optional()?;
    Ok(revision)
}

impl crate::Store {
    /// Appends a revision, making it the current one for its source key.
    ///
    /// Re-appending an existing `(source_key, revision_hash)` pair is
    /// idempotent: it simply marks that row current again.
    pub fn append_revision(&mut self, revision: &NewSourceRevision<'_>) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        tx.execute(
            "UPDATE source_revisions SET is_current = 0 WHERE source_key = ?1 AND is_current = 1",
            params![revision.source_key],
        )?;
        tx.execute(
            "INSERT INTO source_revisions
                (source_key, revision_hash, kind, title, uri, local_path, raw_name, origin_class,
                 metadata_json, ingested_at, is_current)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)
             ON CONFLICT(source_key, revision_hash) DO UPDATE SET is_current = 1",
            params![
                revision.source_key,
                revision.revision_hash,
                revision.kind.as_str(),
                revision.title,
                revision.uri,
                revision.local_path,
                revision.raw_name,
                revision.origin_class.as_str(),
                revision.metadata_json,
                revision.ingested_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns the current revision for `source_key`, if any.
    pub fn current_revision(&self, source_key: &str) -> Result<Option<SourceRevision>, StoreError> {
        query_current(&self.connection, source_key)
    }

    /// Returns the title of the current revision for `source_key`, if any.
    ///
    /// Convenience wrapper over [`crate::Store::current_revision`] for
    /// callers that only need the display title.
    pub fn current_revision_title(&self, source_key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .current_revision(source_key)?
            .and_then(|revision| revision.title))
    }

    /// One trust class per source key, fetched in a single query.
    ///
    /// Mirrors [`Store::current_titles`]; a source missing from the map means
    /// the store has no current revision for it (the column itself is NOT NULL).
    pub fn current_origins(
        &self,
        source_keys: &HashSet<&str>,
    ) -> Result<HashMap<String, OriginClass>, StoreError> {
        if source_keys.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = numbered_placeholders(source_keys);
        let sql = format!(
            "SELECT source_key, origin_class FROM source_revisions
             WHERE is_current = 1 AND source_key IN ({placeholders})"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let params: Vec<&str> = source_keys.iter().copied().collect();
        let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut origins = HashMap::new();
        for row in rows {
            let (key, origin) = row?;
            origins.insert(key, origin_from_string(&origin)?);
        }
        Ok(origins)
    }

    /// Returns display titles for the current revision of each requested
    /// source key, in a single query. Keys without a current revision are
    /// absent from the result.
    pub fn current_titles(
        &self,
        source_keys: &HashSet<&str>,
    ) -> Result<HashMap<String, String>, StoreError> {
        if source_keys.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = numbered_placeholders(source_keys);
        let sql = format!(
            "SELECT source_key, title FROM source_revisions
             WHERE is_current = 1 AND source_key IN ({placeholders})"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let params: Vec<&str> = source_keys.iter().copied().collect();
        let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut titles = HashMap::new();
        for row in rows {
            let (key, title) = row?;
            if let Some(title) = title {
                titles.insert(key, title);
            }
        }
        Ok(titles)
    }

    /// Refreshes the mutable columns of one revision in place.
    ///
    /// Used when a file's bytes did not change but its fingerprint, stored path
    /// or raw spelling moved — a directory move must not leave a stale absolute
    /// path behind (ADR 0024). Never touches `source_key` or `revision_hash`:
    /// identity is not mutable.
    pub fn update_revision_fingerprint(
        &mut self,
        source_key: &str,
        revision_hash: &str,
        metadata_json: &str,
        local_path: &str,
        raw_name: &str,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE source_revisions
             SET metadata_json = ?3, local_path = ?4, raw_name = ?5
             WHERE source_key = ?1 AND revision_hash = ?2",
            params![
                source_key,
                revision_hash,
                metadata_json,
                local_path,
                raw_name
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::RevisionMissing {
                source_key: source_key.to_owned(),
                revision_hash: revision_hash.to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn open_temp_store() -> Store {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("initialize schema");
        store
    }

    fn revision<'a>(source_key: &'a str, revision_hash: &'a str) -> NewSourceRevision<'a> {
        NewSourceRevision {
            source_key,
            revision_hash,
            kind: SourceKind::Note,
            title: Some("Demo"),
            uri: None,
            local_path: None,
            raw_name: Some("Demo.md"),
            origin_class: OriginClass::Owner,
            metadata_json: "{\"fingerprint\":\"x\"}",
            ingested_at: 1_700_000_000,
        }
    }

    fn current_flags(store: &Store, source_key: &str) -> Vec<(String, bool)> {
        let mut statement = store
            .connection
            .prepare(
                "SELECT revision_hash, is_current FROM source_revisions
                 WHERE source_key = ?1 ORDER BY revision_hash",
            )
            .expect("prepare");
        let rows = statement
            .query_map(params![source_key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
            })
            .expect("query");
        rows.map(|row| row.expect("row")).collect()
    }

    #[test]
    fn first_append_is_current() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append");

        let current = store.current_revision("a.md").expect("read").expect("row");
        assert_eq!(current.revision_hash, "hash1");
        assert!(current.is_current);
        assert_eq!(current.kind, SourceKind::Note);
        assert_eq!(current.origin_class, OriginClass::Owner);
        assert_eq!(current.title.as_deref(), Some("Demo"));
        assert_eq!(
            current.raw_name.as_deref(),
            Some("Demo.md"),
            "raw_name round-trips beside the key"
        );
    }

    #[test]
    fn second_revision_demotes_first_but_keeps_it() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append hash1");
        store
            .append_revision(&revision("a.md", "hash2"))
            .expect("append hash2");

        let current = store.current_revision("a.md").expect("read").expect("row");
        assert_eq!(current.revision_hash, "hash2");

        let flags = current_flags(&store, "a.md");
        assert_eq!(flags.len(), 2);
        assert!(flags.contains(&("hash1".to_owned(), false)));
        assert!(flags.contains(&("hash2".to_owned(), true)));
    }

    #[test]
    fn reappending_same_revision_is_idempotent() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append hash1");
        store
            .append_revision(&revision("a.md", "hash2"))
            .expect("append hash2");
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("re-append hash1");

        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM source_revisions
                 WHERE source_key = 'a.md' AND revision_hash = 'hash1'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1, "no duplicate row for the same pair");

        let current = store.current_revision("a.md").expect("read").expect("row");
        assert_eq!(current.revision_hash, "hash1");
        assert!(current.is_current);
    }

    #[test]
    fn missing_source_returns_none() {
        let store = open_temp_store();
        assert!(store.current_revision("nope").expect("read").is_none());
        assert!(
            store
                .current_revision_title("nope")
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn current_revision_title_extracts_title() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append");
        assert_eq!(
            store
                .current_revision_title("a.md")
                .expect("read")
                .as_deref(),
            Some("Demo")
        );
    }

    #[test]
    fn refreshing_a_missing_revision_is_an_error() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append");

        let error = store
            .update_revision_fingerprint("a.md", "missing", "{}", "a.md", "a.md")
            .expect_err("a missing revision must not look like a successful refresh");
        assert!(
            matches!(&error, StoreError::RevisionMissing { revision_hash, .. } if revision_hash == "missing"),
            "{error}"
        );
    }

    #[test]
    fn refresh_updates_exactly_one_row() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append hash1");
        store
            .append_revision(&revision("a.md", "hash2"))
            .expect("append hash2");

        store
            .update_revision_fingerprint(
                "a.md",
                "hash1",
                "{\"updated\":true}",
                "notes/a.md",
                "a.md",
            )
            .expect("update the mutable columns");

        let metadata: Vec<String> = store
            .connection
            .prepare("SELECT metadata_json FROM source_revisions WHERE source_key = 'a.md' ORDER BY revision_hash")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get(0))?
                    .collect()
            })
            .expect("read metadata");
        assert_eq!(
            metadata,
            vec![
                "{\"updated\":true}".to_owned(),
                "{\"fingerprint\":\"x\"}".to_owned()
            ]
        );

        // The refresh is path-aware: a moved file updates both the stored path
        // and the raw spelling, and touches nothing else.
        let refreshed: (Option<String>, Option<String>, String, String) = store
            .connection
            .query_row(
                "SELECT local_path, raw_name, source_key, revision_hash
                 FROM source_revisions WHERE source_key = 'a.md' AND revision_hash = 'hash1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read the refreshed row");
        assert_eq!(
            refreshed,
            (
                Some("notes/a.md".to_owned()),
                Some("a.md".to_owned()),
                "a.md".to_owned(),
                "hash1".to_owned()
            ),
            "identity columns survive a refresh"
        );
    }

    #[test]
    fn unknown_kind_string_is_rejected() {
        let store = open_temp_store();
        store
            .connection
            .execute(
                "INSERT INTO source_revisions
                    (source_key, revision_hash, kind, title, uri, local_path, origin_class,
                     metadata_json, ingested_at, is_current)
                 VALUES ('a.md', 'hash1', 'poem', NULL, NULL, NULL, 'owner', '{}', 0, 1)",
                [],
            )
            .expect("seed bad kind");

        let error = store
            .current_revision("a.md")
            .expect_err("unknown kind must fail");
        // The decode failure is raised inside the row mapper, so it arrives
        // wrapped in the SQLite conversion error.
        let inner = match error {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(boxed)) => {
                *boxed.downcast::<StoreError>().expect("store error inside")
            }
            other => other,
        };
        assert!(matches!(inner, StoreError::InvalidValue { .. }));
    }
}
