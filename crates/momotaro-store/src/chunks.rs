//! Reading and writing derived chunks.

use std::collections::BTreeSet;

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

/// What one replace found and kept: `chunk_id`s retained because a reference
/// still names them, and `chunk_id`s that were referenced but already absent
/// when the replace ran — the trace an out-of-band wipe leaves behind (a ref
/// whose row was never written lands here too) — a wipe must emit drift,
/// not silent re-verification. Both lists are sorted.
#[derive(Debug, PartialEq, Eq)]
pub struct ChunkReplaceReport {
    /// Rows kept as they were because `answer_citations` or
    /// `run_context_items` still names their `chunk_id`.
    pub retained: Vec<String>,
    /// Referenced `chunk_id`s that had no row when the replace ran.
    pub drift: Vec<String>,
}

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

/// The `chunk_id`s that `answer_citations` and `run_context_items` still
/// name for one `(source_key, revision_hash)` — the rows a replace may not
/// delete or overwrite (the retainer set). A name only counts inside
/// its own five-tuple: a reference to another `(source_key, revision_hash)`
/// neither pins a row here nor enters this replace's drift.
///
/// `run_context_items.source_ref_json` is read through `json_valid`, so an
/// unparseable row neither pins anything nor fails the replace; a ref whose
/// JSON names no `chunk_id`, or names one that is not a JSON string, is not
/// a reference either (the five-tuple is the identity, `momotaro-verify`'s
/// citation shape) — alien types are skipped, not errors.
fn referenced_chunk_ids(
    tx: &rusqlite::Transaction<'_>,
    source_key: &str,
    revision_hash: &str,
) -> Result<BTreeSet<String>, StoreError> {
    let mut ids = BTreeSet::new();
    {
        let mut statement = tx.prepare_cached(
            "SELECT DISTINCT chunk_id FROM answer_citations
             WHERE source_key = ?1 AND revision_hash = ?2",
        )?;
        let rows = statement.query_map(params![source_key, revision_hash], |row| {
            row.get::<_, String>(0)
        })?;
        for id in rows {
            ids.insert(id?);
        }
    }
    {
        let mut statement = tx.prepare_cached(
            "SELECT DISTINCT
                    CASE WHEN json_valid(source_ref_json)
                         THEN json_extract(source_ref_json, '$.chunk_id') END
             FROM run_context_items
             WHERE source_ref_json IS NOT NULL
               AND CASE WHEN json_valid(source_ref_json)
                        THEN json_extract(source_ref_json, '$.source_key') END = ?1
               AND CASE WHEN json_valid(source_ref_json)
                        THEN json_extract(source_ref_json, '$.revision_hash') END = ?2",
        )?;
        let rows = statement.query_map(params![source_key, revision_hash], |row| {
            row.get::<_, rusqlite::types::Value>(0)
        })?;
        for id in rows {
            // Only a JSON string names a chunk. `json_extract` hands back
            // numbers and booleans as typed SQL values, and reading those as
            // `String` would fail the whole replace — an alien row must
            // neither pin nor blow up.
            if let rusqlite::types::Value::Text(id) = id? {
                ids.insert(id);
            }
        }
    }
    Ok(ids)
}

/// Replaces one revision's chunks and its source's `graph_links` rows inside
/// the caller's transaction (`chunks` is a retained projection).
///
/// Every row is written under the `source_key` / `revision_hash` arguments —
/// the delete key — so a row struct's own fields cannot disagree with them,
/// the same rule the `graph_links` writer enforces.
///
/// Retention is row-scoped: rows whose `chunk_id` any `answer_citations` or
/// `run_context_items` row still names are kept exactly as they are — never
/// deleted, never overwritten, so the five-tuple's `chunk_hash` cannot move
/// under a citation — while uncited rows of the same revision are replaced
/// away. The returned [`ChunkReplaceReport`] lists the kept ids and any
/// referenced id that was already absent when the replace ran: an out-of-band
/// whole-table wipe surfaces through this path as drift, not as silent
/// re-verification.
pub(crate) fn replace_chunks_tx(
    tx: &rusqlite::Transaction<'_>,
    source_key: &str,
    revision_hash: &str,
    chunks: &[Chunk],
    edges: &[momotaro_contracts::GraphLink],
) -> Result<ChunkReplaceReport, StoreError> {
    let referenced = referenced_chunk_ids(tx, source_key, revision_hash)?;
    let existing: BTreeSet<String> = {
        let mut statement = tx.prepare_cached(
            "SELECT chunk_id FROM chunks WHERE source_key = ?1 AND revision_hash = ?2",
        )?;
        let rows = statement.query_map(params![source_key, revision_hash], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    // One derivation feeds both sides: the delete set and the insert skip are
    // complements of the same two sets, so they cannot disagree.
    let retained: BTreeSet<String> = referenced.intersection(&existing).cloned().collect();
    let drift: Vec<String> = referenced.difference(&existing).cloned().collect();
    for chunk_id in existing.difference(&referenced) {
        tx.execute(
            "DELETE FROM chunks
             WHERE source_key = ?1 AND revision_hash = ?2 AND chunk_id = ?3",
            params![source_key, revision_hash, chunk_id],
        )?;
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO chunks
                (chunk_id, source_key, revision_hash, ordinal, heading_path, text, locator_json,
                 chunk_hash, token_estimate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for chunk in chunks {
            if retained.contains(&chunk.chunk_id) {
                // A cited row is kept as it is; an incoming row under the
                // same id (a new policy's hash) must not move its hash.
                continue;
            }
            insert.execute(params![
                chunk.chunk_id,
                source_key,
                revision_hash,
                chunk.ordinal,
                chunk.heading_path,
                chunk.text,
                chunk.locator_json,
                chunk.chunk_hash,
                chunk.token_estimate,
            ])?;
        }
    }
    crate::graph::write_edges_tx(tx, source_key, edges)?;
    Ok(ChunkReplaceReport {
        retained: retained.into_iter().collect(),
        drift,
    })
}

impl crate::Store {
    /// Atomically replaces one source revision's chunks — except rows an
    /// `answer_citations` or `run_context_items` row still names, which are
    /// retained exactly as they are — and the same source's
    /// `graph_links` rows (the edges ride the file's own content
    /// transaction).
    ///
    /// Returns the retained `chunk_id`s and any referenced id that was
    /// already absent when the replace ran; see [`ChunkReplaceReport`].
    ///
    /// # Errors
    ///
    /// - `StoreError::Sqlite` when the transaction cannot be opened, any
    ///   statement of the replace fails, or the commit fails.
    pub fn replace_chunks(
        &mut self,
        source_key: &str,
        revision_hash: &str,
        chunks: &[Chunk],
        edges: &[momotaro_contracts::GraphLink],
    ) -> Result<ChunkReplaceReport, StoreError> {
        let tx = self.connection.transaction()?;
        let report = replace_chunks_tx(&tx, source_key, revision_hash, chunks, edges)?;
        tx.commit()?;
        Ok(report)
    }

    /// Returns one chunk by its deterministic identifier.
    ///
    /// # Errors
    ///
    /// - `StoreError::Sqlite` when the query fails; a `chunk_id` with no row
    ///   is `Ok(None)`, not an error.
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
    ///
    /// # Errors
    ///
    /// - `StoreError::Sqlite` when the query cannot be prepared or a row
    ///   cannot be read.
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
    ///
    /// # Errors
    ///
    /// - `StoreError::Sqlite` when any of the count queries fails.
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
    use crate::test_support::temp_store;
    use crate::{NewSourceRevision, Store};
    use momotaro_contracts::{OriginClass, SourceKind};

    fn ingest_current(store: &mut Store, source_key: &str, revision_hash: &str) {
        store
            .append_revision(&NewSourceRevision {
                source_key,
                revision_hash,
                kind: SourceKind::Note,
                title: Some("T"),
                uri: None,
                local_path: None,
                raw_name: None,
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

    /// Plants one `answer_citations` row naming the five-tuple fields.
    fn plant_answer_citation(
        store: &Store,
        source_key: &str,
        revision_hash: &str,
        chunk_id: &str,
        chunk_hash: &str,
    ) {
        store
            .connection
            .execute(
                "INSERT INTO answer_citations
                    (answer_id, citation_index, source_key, revision_hash, chunk_id, chunk_hash,
                     locator_json, quoted_text, verified)
                 VALUES ('ans', 0, ?1, ?2, ?3, ?4, '{\"start_line\":0}', 'quote', 1)",
                params![source_key, revision_hash, chunk_id, chunk_hash],
            )
            .expect("plant citation");
    }

    /// Plants one `run_context_items` row whose `source_ref_json` is the
    /// six-key shape `evidence_source_ref` writes (the five-tuple plus the
    /// origin class). `json_object` builds it, so the fixture cannot
    /// hand-roll invalid JSON.
    fn plant_context_ref(
        store: &Store,
        source_key: &str,
        revision_hash: &str,
        chunk_id: &str,
        chunk_hash: &str,
    ) {
        store
            .connection
            .execute(
                "INSERT INTO run_context_items
                    (run_id, ordinal, fragment_id, kind, role, content_kind, body,
                     source_ref_json, token_estimate, body_hash)
                 VALUES ('run1', 0, 'f0001', 'evidence', 'user', 'text', 'body',
                         json_object('source_key', ?1, 'revision_hash', ?2,
                                     'chunk_id', ?3, 'chunk_hash', ?4,
                                     'locator_json', '{\"start_line\":0}',
                                     'origin_class', 'owner'),
                         3, 'bh')",
                params![source_key, revision_hash, chunk_id, chunk_hash],
            )
            .expect("plant context ref");
    }

    #[test]
    fn replace_chunks_writes_and_replaces() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");

        let first = vec![
            chunk("c1", "a.md", "hash1", 0),
            chunk("c2", "a.md", "hash1", 1),
            chunk("c3", "a.md", "hash1", 2),
        ];
        store
            .replace_chunks("a.md", "hash1", &first, &[])
            .expect("first write");
        assert_eq!(chunk_count(&store, "a.md", "hash1"), 3);

        let second = vec![
            chunk("c1", "a.md", "hash1", 0),
            chunk("c4", "a.md", "hash1", 1),
        ];
        store
            .replace_chunks("a.md", "hash1", &second, &[])
            .expect("replace");
        assert_eq!(chunk_count(&store, "a.md", "hash1"), 2);

        store
            .replace_chunks("a.md", "hash1", &second, &[])
            .expect("replace again");
        assert_eq!(chunk_count(&store, "a.md", "hash1"), 2, "idempotent");
    }

    #[test]
    fn get_chunk_round_trips() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        let expected = chunk("c1", "a.md", "hash1", 0);
        store
            .replace_chunks("a.md", "hash1", std::slice::from_ref(&expected), &[])
            .expect("write");

        let loaded = store.get_chunk("c1").expect("read").expect("exists");
        assert_eq!(loaded, expected);
        assert!(store.get_chunk("missing").expect("read").is_none());
    }

    #[test]
    fn all_current_chunks_filters_and_orders() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "b.md", "b2");
        ingest_current(&mut store, "a.md", "a1");
        ingest_current(&mut store, "a.md", "a2");

        // Stale chunks for superseded revisions must be excluded.
        store
            .replace_chunks("a.md", "a1", &[chunk("stale", "a.md", "a1", 0)], &[])
            .expect("stale write");
        store
            .replace_chunks(
                "a.md",
                "a2",
                &[chunk("ca1", "a.md", "a2", 1), chunk("ca0", "a.md", "a2", 0)],
                &[],
            )
            .expect("a2 chunks");
        store
            .replace_chunks("b.md", "b2", &[chunk("cb0", "b.md", "b2", 0)], &[])
            .expect("b2 chunks");

        let loaded = store.all_current_chunks().expect("read");
        let ids: Vec<&str> = loaded.iter().map(|c| c.chunk_id.as_str()).collect();
        assert_eq!(ids, vec!["ca0", "ca1", "cb0"]);
    }

    #[test]
    fn counts_reflect_rows() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "a1");
        ingest_current(&mut store, "a.md", "a2");
        ingest_current(&mut store, "b.md", "b1");

        store
            .replace_chunks("a.md", "a2", &[chunk("ca0", "a.md", "a2", 0)], &[])
            .expect("chunks");

        let counts = store.counts().expect("counts");
        assert_eq!(counts.sources, 2);
        assert_eq!(counts.revisions, 3);
        assert_eq!(counts.chunks, 1);
    }

    #[test]
    fn counts_are_zero_on_fresh_schema() {
        let (_dir, store) = temp_store();
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

    /// Contract: rows a reference names
    /// stay. Fixture ids stand for `hash(sk, rh, ordinal)`; a coalescing
    /// policy shrinks the revision to one chunk, so the cited chunks at
    /// ordinals 1 and 2 are absent from the incoming set — only retention can
    /// keep the two five-tuples from silently going invalid. One reference
    /// per citing table, so dropping either table's half of the query loses a
    /// named row and goes red.
    #[test]
    fn a_referenced_chunk_survives_a_reingest_that_drops_it() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks(
                "a.md",
                "hash1",
                &[
                    chunk("c0", "a.md", "hash1", 0),
                    chunk("c1", "a.md", "hash1", 1),
                    chunk("c2", "a.md", "hash1", 2),
                ],
                &[],
            )
            .expect("first write");
        // Planted in reverse id order so a report built in query order could
        // not pass for a sorted one.
        plant_answer_citation(&store, "a.md", "hash1", "c2", "hash-c2");
        plant_context_ref(&store, "a.md", "hash1", "c1", "hash-c1");

        let report = store
            .replace_chunks("a.md", "hash1", &[chunk("c0", "a.md", "hash1", 0)], &[])
            .expect("re-ingest under a coalescing policy");

        for (id, hash) in [("c1", "hash-c1"), ("c2", "hash-c2")] {
            let row = store
                .get_chunk(id)
                .expect("read")
                .expect("a referenced chunk stays");
            assert_eq!(
                row.chunk_hash, hash,
                "{id} keeps the hash its five-tuple names"
            );
        }
        assert!(
            store.get_chunk("c0").expect("read").is_some(),
            "the new ingest still lands"
        );
        assert_eq!(
            report.retained,
            ["c1", "c2"],
            "the two cited rows are the ones kept"
        );
        assert!(
            report.drift.is_empty(),
            "a healthy replace reports no drift"
        );
    }

    /// Contract: `chunk_hash` is policy, `chunk_id`
    /// is not — a new hash function re-ingests the same identifiers with new
    /// hashes. The cited row's hash must not move under the citation: the
    /// five-tuple's `chunk_hash` may not silently go invalid.
    #[test]
    fn a_reingest_may_not_rewrite_a_referenced_chunks_hash() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks(
                "a.md",
                "hash1",
                &[
                    chunk("c0", "a.md", "hash1", 0),
                    chunk("c1", "a.md", "hash1", 1),
                ],
                &[],
            )
            .expect("first write");
        plant_answer_citation(&store, "a.md", "hash1", "c0", "hash-c0");

        let mut tampered = chunk("c0", "a.md", "hash1", 0);
        tampered.chunk_hash = "hash-c0-tampered".to_owned();
        let report = store
            .replace_chunks(
                "a.md",
                "hash1",
                &[tampered, chunk("c1", "a.md", "hash1", 1)],
                &[],
            )
            .expect("re-ingest under a new hash function");

        let row = store.get_chunk("c0").expect("read").expect("kept");
        assert_eq!(
            row.chunk_hash, "hash-c0",
            "the five-tuple's hash survives the re-ingest"
        );
        assert_eq!(report.retained, ["c0"]);
        assert!(report.drift.is_empty());
    }

    /// Deleting unreferenced rows is allowed. Retention pins
    /// named rows, not the whole revision — an uncited sibling goes when the
    /// replace replaces it.
    #[test]
    fn deleting_an_unreferenced_chunk_is_still_allowed() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks(
                "a.md",
                "hash1",
                &[
                    chunk("c0", "a.md", "hash1", 0),
                    chunk("c1", "a.md", "hash1", 1),
                ],
                &[],
            )
            .expect("first write");
        plant_context_ref(&store, "a.md", "hash1", "c0", "hash-c0");

        let report = store
            .replace_chunks("a.md", "hash1", &[chunk("c0", "a.md", "hash1", 0)], &[])
            .expect("re-ingest");

        assert!(
            store.get_chunk("c0").expect("read").is_some(),
            "the cited row stays"
        );
        assert!(
            store.get_chunk("c1").expect("read").is_none(),
            "the uncited sibling goes"
        );
        assert_eq!(report.retained, ["c0"]);
        assert!(report.drift.is_empty());
    }

    /// Contract: wiping the table is disaster recovery with the current
    /// policy and must emit drift, not silent re-verification. The
    /// recovery ingest reproduces the rows byte for byte, so every five-tuple
    /// would verify again — the replace must still say the references lost
    /// their rows.
    #[test]
    fn a_whole_table_wipe_is_reported_as_drift_not_silent_reverification() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks(
                "a.md",
                "hash1",
                &[
                    chunk("c0", "a.md", "hash1", 0),
                    chunk("c1", "a.md", "hash1", 1),
                ],
                &[],
            )
            .expect("first write");
        plant_answer_citation(&store, "a.md", "hash1", "c0", "hash-c0");
        plant_context_ref(&store, "a.md", "hash1", "c1", "hash-c1");

        store
            .connection
            .execute("DELETE FROM chunks", [])
            .expect("external whole-table wipe");

        let report = store
            .replace_chunks(
                "a.md",
                "hash1",
                &[
                    chunk("c0", "a.md", "hash1", 0),
                    chunk("c1", "a.md", "hash1", 1),
                ],
                &[],
            )
            .expect("recovery ingest");

        assert_eq!(
            report.drift,
            ["c0", "c1"],
            "the replace reports the references that lost their rows"
        );
        assert!(
            report.retained.is_empty(),
            "there was nothing left to retain"
        );
        assert!(
            store.get_chunk("c0").expect("read").is_some(),
            "recovery still proceeds"
        );
    }

    /// A row whose `source_ref_json` does not parse cannot name a chunk: it
    /// must not pin anything, and it must not fail the replace either (the
    /// extractor reads through `json_valid`).
    #[test]
    fn a_malformed_context_ref_neither_pins_nor_breaks_the_replace() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks("a.md", "hash1", &[chunk("c0", "a.md", "hash1", 0)], &[])
            .expect("first write");
        store
            .connection
            .execute(
                "INSERT INTO run_context_items
                    (run_id, ordinal, fragment_id, kind, role, content_kind, body,
                     source_ref_json, token_estimate, body_hash)
                 VALUES ('run1', 0, 'f0001', 'evidence', 'user', 'text', 'body',
                         'not json at all {', 3, 'bh')",
                [],
            )
            .expect("plant malformed ref");

        let report = store
            .replace_chunks("a.md", "hash1", &[chunk("c1", "a.md", "hash1", 0)], &[])
            .expect("replace goes through");
        assert!(
            store.get_chunk("c0").expect("read").is_none(),
            "nothing is pinned by garbage"
        );
        assert!(report.retained.is_empty());
        assert!(report.drift.is_empty());
    }

    /// A valid-JSON ref whose `chunk_id` is not a JSON string names nothing:
    /// it must not pin, must not enter drift, and must not fail the replace —
    /// `json_extract` returns numbers and booleans as typed SQL values, and
    /// reading those as text would poison every future replace of the pair.
    #[test]
    fn an_alien_typed_context_ref_neither_pins_nor_breaks_the_replace() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks("a.md", "hash1", &[chunk("c0", "a.md", "hash1", 0)], &[])
            .expect("first write");
        for (run, alien) in [
            (
                "run-num",
                "{\"source_key\":\"a.md\",\"revision_hash\":\"hash1\",\"chunk_id\":5}",
            ),
            (
                "run-bool",
                "{\"source_key\":\"a.md\",\"revision_hash\":\"hash1\",\"chunk_id\":true}",
            ),
            (
                "run-none",
                "{\"source_key\":\"a.md\",\"revision_hash\":\"hash1\"}",
            ),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO run_context_items
                        (run_id, ordinal, fragment_id, kind, role, content_kind, body,
                         source_ref_json, token_estimate, body_hash)
                     VALUES (?1, 0, 'f0001', 'evidence', 'user', 'text', 'body', ?2, 3, 'bh')",
                    params![run, alien],
                )
                .expect("plant alien ref");
        }

        let report = store
            .replace_chunks("a.md", "hash1", &[chunk("c1", "a.md", "hash1", 0)], &[])
            .expect("replace goes through");
        assert!(
            store.get_chunk("c0").expect("read").is_none(),
            "an alien name pins nothing"
        );
        assert!(report.retained.is_empty());
        assert!(
            report.drift.is_empty(),
            "an alien name is not a reference to lose: {report:?}"
        );
    }

    /// Retrieval hits do not pin rows. The audit of what a query
    /// once returned must not keep chunks alive — pinning every searched
    /// chunk would make deleting unreferenced rows impossible.
    #[test]
    fn retrieval_hits_do_not_pin_rows() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        store
            .replace_chunks("a.md", "hash1", &[chunk("c0", "a.md", "hash1", 0)], &[])
            .expect("first write");
        store
            .connection
            .execute(
                "INSERT INTO retrieval_hits
                    (retrieval_id, rank, source_key, revision_hash, chunk_id, chunk_hash,
                     locator_json, score, included, omitted_reason)
                 VALUES ('r1', 1, 'a.md', 'hash1', 'c0', 'hash-c0',
                         '{\"start_line\":0}', 1.0, 1, NULL)",
                [],
            )
            .expect("plant retrieval hit");

        let report = store
            .replace_chunks("a.md", "hash1", &[chunk("c1", "a.md", "hash1", 0)], &[])
            .expect("re-ingest");

        assert!(
            store.get_chunk("c0").expect("read").is_none(),
            "a retrieval hit does not pin its chunk"
        );
        assert!(report.retained.is_empty());
        assert!(report.drift.is_empty());
    }

    /// The retainer set is scoped to the pair being replaced: a five-tuple
    /// naming another `(source_key, revision_hash)` neither pins rows here
    /// nor enters this replace's drift (the WHERE filters are load-bearing
    /// against foreign rows).
    #[test]
    fn a_reference_to_another_pair_never_pins_and_never_drifts_here() {
        let (_dir, mut store) = temp_store();
        ingest_current(&mut store, "a.md", "hash1");
        ingest_current(&mut store, "b.md", "hash2");
        store
            .replace_chunks("a.md", "hash1", &[chunk("ca", "a.md", "hash1", 0)], &[])
            .expect("pair a");
        store
            .replace_chunks("b.md", "hash2", &[chunk("cb", "b.md", "hash2", 0)], &[])
            .expect("pair b");
        plant_answer_citation(&store, "b.md", "hash2", "cb", "hash-cb");
        plant_context_ref(&store, "b.md", "hash2", "cb", "hash-cb");

        let report = store
            .replace_chunks("a.md", "hash1", &[chunk("ca2", "a.md", "hash1", 0)], &[])
            .expect("replace pair a");

        assert!(
            store.get_chunk("cb").expect("read").is_some(),
            "pair b's row is untouched"
        );
        assert!(
            store.get_chunk("ca").expect("read").is_none(),
            "pair a's unreferenced row goes"
        );
        assert!(
            report.retained.is_empty(),
            "the foreign name pins nothing here"
        );
        assert!(
            report.drift.is_empty(),
            "the foreign name is not this pair's missing row: {report:?}"
        );
    }
}
