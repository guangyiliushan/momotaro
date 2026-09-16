//! Reading and writing derived chunks.

use momotaro_contracts::Chunk;
use rusqlite::{OptionalExtension, params};

use crate::StoreError;

/// Row counts used by the `stats` projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCounts {
    /// Distinct source keys in `source_revisions`.
    pub sources: u64,
    /// Total rows in `source_revisions`.
    pub revisions: u64,
    /// Total rows in `chunks`.
    pub chunks: u64,
}

const CHUNK_COLUMNS: &str = "chunk_id, source_key, revision_hash, ordinal, heading_path, text, locator_json, \
     chunk_hash, token_estimate";

fn read_chunk_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chunk> {
    Ok(Chunk {
        chunk_id: row.get("chunk_id")?,
        source_key: row.get("source_key")?,
        revision_hash: row.get("revision_hash")?,
        ordinal: row.get("ordinal")?,
        heading_path: row.get("heading_path")?,
        text: row.get("text")?,
        locator_json: row.get("locator_json")?,
        chunk_hash: row.get("chunk_hash")?,
        token_estimate: row.get("token_estimate")?,
    })
}

impl crate::Store {
    /// Atomically replaces every chunk of one source revision.
    pub fn replace_chunks(
        &mut self,
        source_key: &str,
        revision_hash: &str,
        chunks: &[Chunk],
    ) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        tx.execute(
            "DELETE FROM chunks WHERE source_key = ?1 AND revision_hash = ?2",
            params![source_key, revision_hash],
        )?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO chunks
                    (chunk_id, source_key, revision_hash, ordinal, heading_path, text, locator_json,
                     chunk_hash, token_estimate)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for chunk in chunks {
                insert.execute(params![
                    chunk.chunk_id,
                    chunk.source_key,
                    chunk.revision_hash,
                    chunk.ordinal,
                    chunk.heading_path,
                    chunk.text,
                    chunk.locator_json,
                    chunk.chunk_hash,
                    chunk.token_estimate,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns one chunk by its deterministic identifier.
    pub fn get_chunk(&self, chunk_id: &str) -> Result<Option<Chunk>, StoreError> {
        self.connection
            .query_row(
                &format!("SELECT {CHUNK_COLUMNS} FROM chunks WHERE chunk_id = ?1"),
                params![chunk_id],
                read_chunk_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Returns every chunk whose owning revision is the current one.
    ///
    /// Ordered by `(source_key, ordinal)`.
    pub fn all_current_chunks(&self) -> Result<Vec<Chunk>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT c.*
             FROM chunks c
             JOIN source_revisions s
               ON c.source_key = s.source_key AND c.revision_hash = s.revision_hash
             WHERE s.is_current = 1
             ORDER BY c.source_key, c.ordinal",
        )?;
        let rows = statement.query_map([], read_chunk_row)?;
        rows.map(|row| row.map_err(StoreError::from)).collect()
    }

    /// Returns row counts used by the `stats` projection.
    pub fn counts(&self) -> Result<SourceCounts, StoreError> {
        let sources = self.connection.query_row(
            "SELECT COUNT(DISTINCT source_key) FROM source_revisions",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let revisions =
            self.connection
                .query_row("SELECT COUNT(*) FROM source_revisions", [], |row| {
                    row.get::<_, i64>(0)
                })?;
        let chunks = self
            .connection
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| {
                row.get::<_, i64>(0)
            })?;

        Ok(SourceCounts {
            sources: u64::try_from(sources).unwrap_or(0),
            revisions: u64::try_from(revisions).unwrap_or(0),
            chunks: u64::try_from(chunks).unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewSourceRevision, Store};
    use momotaro_contracts::{OriginClass, SourceKind};

    fn open_temp_store() -> Store {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("initialize schema");
        store
    }

    fn ingest_current(store: &mut Store, source_key: &str, revision_hash: &str) {
        store
            .append_revision(&NewSourceRevision {
                source_key,
                revision_hash,
                kind: SourceKind::Note,
                title: Some("T"),
                uri: None,
                local_path: None,
                origin_class: OriginClass::Owner,
                metadata_json: "{}",
                ingested_at: 1,
            })
            .expect("append revision");
    }

    fn chunk(id: &str, source_key: &str, revision_hash: &str, ordinal: u32) -> Chunk {
        Chunk {
            chunk_id: id.to_owned(),
            source_key: source_key.to_owned(),
            revision_hash: revision_hash.to_owned(),
            ordinal,
            heading_path: Some("H1 > H2".to_owned()),
            text: format!("text of {id}"),
            locator_json: "{\"start_line\":0,\"end_line\":1}".to_owned(),
            chunk_hash: format!("hash-{id}"),
            token_estimate: 3,
        }
    }

    fn chunk_count(store: &Store, source_key: &str, revision_hash: &str) -> u64 {
        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE source_key = ?1 AND revision_hash = ?2",
                params![source_key, revision_hash],
                |row| row.get(0),
            )
            .expect("count");
        u64::try_from(count).unwrap_or(0)
    }

    #[test]
    fn replace_chunks_writes_and_replaces() {
        let mut store = open_temp_store();
        ingest_current(&mut store, "a.md", "hash1");

        let first = vec![
            chunk("c1", "a.md", "hash1", 0),
            chunk("c2", "a.md", "hash1", 1),
            chunk("c3", "a.md", "hash1", 2),
        ];
        store
            .replace_chunks("a.md", "hash1", &first)
            .expect("first write");
        assert_eq!(chunk_count(&store, "a.md", "hash1"), 3);

        let second = vec![
            chunk("c1", "a.md", "hash1", 0),
            chunk("c4", "a.md", "hash1", 1),
        ];
        store
            .replace_chunks("a.md", "hash1", &second)
            .expect("replace");
        assert_eq!(chunk_count(&store, "a.md", "hash1"), 2);

        store
            .replace_chunks("a.md", "hash1", &second)
            .expect("replace again");
        assert_eq!(chunk_count(&store, "a.md", "hash1"), 2, "idempotent");
    }

    #[test]
    fn get_chunk_round_trips() {
        let mut store = open_temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        let expected = chunk("c1", "a.md", "hash1", 0);
        store
            .replace_chunks("a.md", "hash1", std::slice::from_ref(&expected))
            .expect("write");

        let loaded = store.get_chunk("c1").expect("read").expect("exists");
        assert_eq!(loaded, expected);
        assert!(store.get_chunk("missing").expect("read").is_none());
    }

    #[test]
    fn all_current_chunks_filters_and_orders() {
        let mut store = open_temp_store();
        ingest_current(&mut store, "b.md", "b2");
        ingest_current(&mut store, "a.md", "a1");
        ingest_current(&mut store, "a.md", "a2");

        // Stale chunks for superseded revisions must be excluded.
        store
            .replace_chunks("a.md", "a1", &[chunk("stale", "a.md", "a1", 0)])
            .expect("stale write");
        store
            .replace_chunks(
                "a.md",
                "a2",
                &[chunk("ca1", "a.md", "a2", 1), chunk("ca0", "a.md", "a2", 0)],
            )
            .expect("a2 chunks");
        store
            .replace_chunks("b.md", "b2", &[chunk("cb0", "b.md", "b2", 0)])
            .expect("b2 chunks");

        let loaded = store.all_current_chunks().expect("read");
        let ids: Vec<&str> = loaded.iter().map(|c| c.chunk_id.as_str()).collect();
        assert_eq!(ids, vec!["ca0", "ca1", "cb0"]);
    }

    #[test]
    fn counts_reflect_rows() {
        let mut store = open_temp_store();
        ingest_current(&mut store, "a.md", "a1");
        ingest_current(&mut store, "a.md", "a2");
        ingest_current(&mut store, "b.md", "b1");

        store
            .replace_chunks("a.md", "a2", &[chunk("ca0", "a.md", "a2", 0)])
            .expect("chunks");

        let counts = store.counts().expect("counts");
        assert_eq!(counts.sources, 2);
        assert_eq!(counts.revisions, 3);
        assert_eq!(counts.chunks, 1);
    }

    #[test]
    fn counts_are_zero_on_fresh_schema() {
        let store = open_temp_store();
        let counts = store.counts().expect("counts");
        assert_eq!(
            counts,
            SourceCounts {
                sources: 0,
                revisions: 0,
                chunks: 0,
            }
        );
    }
}
