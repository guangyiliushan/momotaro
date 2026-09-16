//! Reading and appending canonical source revisions.

use momotaro_contracts::{OriginClass, SourceKind, SourceRevision};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{StoreError, invalid_value};

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
    /// Absolute local path, when the source lives on disk.
    pub local_path: Option<&'a str>,
    /// Trust origin.
    pub origin_class: OriginClass,
    /// JSON metadata blob (fingerprint, tool tag, provider data).
    pub metadata_json: &'a str,
    /// Unix epoch seconds at ingestion.
    pub ingested_at: i64,
}

pub(crate) fn kind_to_string(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Note => "note",
        SourceKind::Paper => "paper",
        SourceKind::Web => "web",
    }
}

pub(crate) fn kind_from_string(value: &str) -> Result<SourceKind, StoreError> {
    match value {
        "note" => Ok(SourceKind::Note),
        "paper" => Ok(SourceKind::Paper),
        "web" => Ok(SourceKind::Web),
        other => Err(invalid_value("kind", other)),
    }
}

pub(crate) fn origin_to_string(origin: OriginClass) -> &'static str {
    match origin {
        OriginClass::Owner => "owner",
        OriginClass::Paper => "paper",
        OriginClass::Web => "web",
        OriginClass::Agent => "agent",
        OriginClass::System => "system",
    }
}

pub(crate) fn origin_from_string(value: &str) -> Result<OriginClass, StoreError> {
    match value {
        "owner" => Ok(OriginClass::Owner),
        "paper" => Ok(OriginClass::Paper),
        "web" => Ok(OriginClass::Web),
        "agent" => Ok(OriginClass::Agent),
        "system" => Ok(OriginClass::System),
        other => Err(invalid_value("origin_class", other)),
    }
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
            "SELECT source_key, revision_hash, kind, title, uri, local_path, origin_class,
                    metadata_json, ingested_at, is_current
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
                (source_key, revision_hash, kind, title, uri, local_path, origin_class,
                 metadata_json, ingested_at, is_current)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1)
             ON CONFLICT(source_key, revision_hash) DO UPDATE SET is_current = 1",
            params![
                revision.source_key,
                revision.revision_hash,
                kind_to_string(revision.kind),
                revision.title,
                revision.uri,
                revision.local_path,
                origin_to_string(revision.origin_class),
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

    /// Returns display titles for the current revision of each requested
    /// source key, in a single query. Keys without a current revision are
    /// absent from the result.
    pub fn current_titles(
        &self,
        source_keys: &std::collections::HashSet<&str>,
    ) -> Result<std::collections::HashMap<String, String>, StoreError> {
        if source_keys.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = source_keys
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT source_key, title FROM source_revisions
             WHERE is_current = 1 AND source_key IN ({placeholders})"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let params: Vec<&str> = source_keys.iter().copied().collect();
        let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut titles = std::collections::HashMap::new();
        for row in rows {
            let (key, title) = row?;
            if let Some(title) = title {
                titles.insert(key, title);
            }
        }
        Ok(titles)
    }

    /// Replaces the metadata blob of one specific revision.
    pub fn update_revision_metadata(
        &mut self,
        source_key: &str,
        revision_hash: &str,
        metadata_json: &str,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "UPDATE source_revisions SET metadata_json = ?3
             WHERE source_key = ?1 AND revision_hash = ?2",
            params![source_key, revision_hash, metadata_json],
        )?;
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
    fn metadata_update_targets_one_revision() {
        let mut store = open_temp_store();
        store
            .append_revision(&revision("a.md", "hash1"))
            .expect("append hash1");
        store
            .append_revision(&revision("a.md", "hash2"))
            .expect("append hash2");

        store
            .update_revision_metadata("a.md", "hash1", "{\"updated\":true}")
            .expect("update metadata");

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
