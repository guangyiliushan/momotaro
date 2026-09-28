//! One file's ingest unit: revision, chunks, edges, and the file's event in a
//! single `BEGIN IMMEDIATE` (D40's unit — the API ADR 0026 registered as
//! follow-up work).

use momotaro_contracts::{Chunk, GraphLink};
use rusqlite::TransactionBehavior;

use crate::events::NewStoreEvent;
use crate::revisions::NewSourceRevision;
use crate::{Store, StoreError, invalid_value};

impl Store {
    /// Commits one file's ingest unit in exactly one `BEGIN IMMEDIATE`.
    ///
    /// Order inside the transaction is fixed: `guard` runs first (W2.5
    /// re-checks the file's stat there; a refusal rolls the whole unit back),
    /// then the revision upsert, the wholesale replacement of the revision's
    /// chunks and of the source's `graph_links` rows (D40 / ADR 0027), and
    /// finally the file's event when the caller supplies one. Nothing is
    /// written unless the event lands with it, so a crash between data and
    /// event cannot exist inside this unit. A guard that wants its refusal
    /// distinguishable from a write failure returns
    /// [`StoreError::GuardRejected`]; a caller with no end-of-transaction
    /// check to make (the index batch) passes a no-op.
    ///
    /// The event must name the revision's source: a row about another source
    /// riding this unit would claim work that is not here (ADR 0026). The data
    /// half replays idempotently (revision upsert; chunks and edges replaced
    /// wholesale); the event is append-only like every event, and replay
    /// semantics for it stay the caller's (`batch_id` / `seq` / `external_id`).
    pub fn commit_source_file(
        &mut self,
        revision: &NewSourceRevision<'_>,
        chunks: &[Chunk],
        edges: &[GraphLink],
        event: Option<&NewStoreEvent<'_>>,
        guard: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if let Some(event) = event
            && event.source_key != Some(revision.source_key)
        {
            return Err(invalid_value(
                "source_key",
                event.source_key.unwrap_or("<none>"),
            ));
        }
        // Transaction time is read outside the write transaction (ADR 0026);
        // the guard below is the transaction's first step.
        let pending_event = match event {
            Some(event) => Some((event, self.next_recorded_at()?)),
            None => None,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard()?;
        crate::revisions::append_revision_tx(&tx, revision)?;
        crate::chunks::replace_chunks_tx(
            &tx,
            revision.source_key,
            revision.revision_hash,
            chunks,
            edges,
        )?;
        if let Some((event, recorded_at)) = pending_event {
            crate::events::insert_events_tx(
                &tx,
                event.batch_id,
                std::slice::from_ref(event),
                recorded_at,
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use momotaro_contracts::{EdgeKind, LinkSource, OriginClass, SourceKind};

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("init schema");
        (dir, store)
    }

    fn revision<'a>(source_key: &'a str, revision_hash: &'a str) -> NewSourceRevision<'a> {
        NewSourceRevision {
            source_key,
            revision_hash,
            kind: SourceKind::Note,
            title: Some("T"),
            uri: None,
            local_path: Some("a.md"),
            raw_name: Some("a.md"),
            origin_class: OriginClass::Owner,
            metadata_json: "{}",
            ingested_at: 7,
        }
    }

    fn chunk(id: &str, source_key: &str, revision_hash: &str) -> Chunk {
        Chunk {
            chunk_id: id.to_owned(),
            source_key: source_key.to_owned(),
            revision_hash: revision_hash.to_owned(),
            ordinal: 0,
            heading_path: None,
            text: "body".to_owned(),
            locator_json: "{\"start_line\":0,\"end_line\":1}".to_owned(),
            chunk_hash: format!("hash-{id}"),
            token_estimate: 1,
        }
    }

    fn edge(from: &str, raw_target: &str) -> GraphLink {
        GraphLink {
            from_source_key: from.to_owned(),
            to_source_key: None,
            raw_target: raw_target.to_owned(),
            edge_kind: EdgeKind::Link,
            link_source: LinkSource::Wikilink,
            anchor_kind: None,
            anchor_raw: None,
            created_at: 7,
        }
    }

    fn event<'a>(batch_id: &'a str, seq: i64, source_key: Option<&'a str>) -> NewStoreEvent<'a> {
        NewStoreEvent {
            occurred_at: 1_000,
            batch_id,
            seq,
            domain: "ingest",
            event_type: "ingest.coverage",
            source_key,
            outcome: Some("indexed"),
            reason_code: None,
            message: None,
            data_json: "{}",
            external_id: None,
        }
    }

    fn scalar(store: &Store, sql: &str) -> i64 {
        store
            .connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar")
    }

    /// The lost-event window cannot open: when the file's event cannot land,
    /// the data half does not survive it. The old shape — content appends
    /// first, the event later — leaves revision, chunks and edges committed
    /// here; that is the window this unit exists to close, and this test is
    /// its witness.
    #[test]
    fn an_event_that_cannot_land_takes_the_data_with_it() {
        let (_dir, mut store) = temp_store();
        // The same (batch_id, seq): the unit's event insert must fail.
        store
            .append_store_event(&event("batch", 1, Some("note:a")))
            .expect("plant the conflict");
        let result = store.commit_source_file(
            &revision("note:a", "h1"),
            &[chunk("c1", "note:a", "h1")],
            &[edge("note:a", "B")],
            Some(&event("batch", 1, Some("note:a"))),
            || Ok(()),
        );
        assert!(result.is_err(), "the UNIQUE (batch_id, seq) must fire");
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
            0,
            "the revision rolled back with the event"
        );
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM chunks"),
            0,
            "the chunks rolled back with the event"
        );
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM graph_links"),
            0,
            "the edges rolled back with the event"
        );
    }

    /// A failure halfway through the unit takes revision, chunks and edges
    /// down together — three tables, one fate — without touching rows the
    /// unit never owned.
    #[test]
    fn a_failure_mid_unit_rolls_back_revision_chunks_and_edges() {
        let (_dir, mut store) = temp_store();
        // A chunk id already owned by another source: the unit's INSERT
        // collides after its revision write.
        store
            .replace_chunks("note:other", "h0", &[chunk("c1", "note:other", "h0")], &[])
            .expect("plant the conflict");
        let result = store.commit_source_file(
            &revision("note:a", "h1"),
            &[chunk("c1", "note:a", "h1")],
            &[edge("note:a", "B")],
            None,
            || Ok(()),
        );
        assert!(result.is_err(), "chunk_id is the primary key");
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
            0,
            "the unit's revision rolled back with its chunks"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM chunks WHERE source_key = 'note:a'"
            ),
            0,
            "no chunk of the failed unit survived"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE from_source_key = 'note:a'"
            ),
            0,
            "no edge of the failed unit survived"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM chunks WHERE source_key = 'note:other'"
            ),
            1,
            "the rollback leaves rows the unit never owned alone"
        );
    }

    /// Replaying the unit is idempotent: the same input twice leaves the same
    /// rows — the data half is upsert plus wholesale replace.
    #[test]
    fn replaying_the_same_unit_leaves_the_same_rows() {
        let (_dir, mut store) = temp_store();
        let rev = revision("note:a", "h1");
        let chunks = vec![chunk("c1", "note:a", "h1"), chunk("c2", "note:a", "h1")];
        let edges = vec![edge("note:a", "B"), edge("note:a", "C")];
        store
            .commit_source_file(&rev, &chunks, &edges, None, || Ok(()))
            .expect("first");
        let revision_before = store
            .current_revision("note:a")
            .expect("read")
            .expect("row");
        let chunks_before = store.all_current_chunks().expect("read");
        let edges_before = store.all_graph_links().expect("read");
        store
            .commit_source_file(&rev, &chunks, &edges, None, || Ok(()))
            .expect("second");
        assert_eq!(
            store
                .current_revision("note:a")
                .expect("read")
                .expect("row"),
            revision_before,
            "the revision row is the same row"
        );
        assert_eq!(
            store.all_current_chunks().expect("read"),
            chunks_before,
            "the chunks are the same rows"
        );
        assert_eq!(
            store.all_graph_links().expect("read"),
            edges_before,
            "the edges are the same rows"
        );
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
            1,
            "one revision row, not two"
        );
    }

    /// The attachment shape — no chunks, no edges — lands the revision and
    /// touches nothing else: the wholesale deletes find no rows of this source
    /// and leave every other source's rows alone.
    #[test]
    fn an_empty_unit_lands_only_the_revision() {
        let (_dir, mut store) = temp_store();
        store
            .commit_source_file(
                &revision("note:other", "h0"),
                &[chunk("c1", "note:other", "h0")],
                &[edge("note:other", "B")],
                None,
                || Ok(()),
            )
            .expect("plant a foreign source");
        store
            .commit_source_file(&revision("note:a", "h1"), &[], &[], None, || Ok(()))
            .expect("the empty unit commits");
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
            2,
            "the revision landed beside the planted one"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM chunks WHERE source_key = 'note:a'"
            ),
            0,
            "no chunks were invented"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE from_source_key = 'note:a'"
            ),
            0,
            "no edges were invented"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM chunks WHERE source_key = 'note:other'"
            ),
            1,
            "the empty unit's deletes do not reach other sources"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE from_source_key = 'note:other'"
            ),
            1,
            "and do not reach their edges either"
        );
    }

    /// Chunk rows are written under the unit's keys: a struct whose own fields
    /// disagree cannot smuggle rows under another key — out of the replay's
    /// wholesale replace, where they would accumulate.
    #[test]
    fn chunk_rows_are_written_under_the_units_keys() {
        let (_dir, mut store) = temp_store();
        store
            .commit_source_file(
                &revision("note:a", "h1"),
                &[chunk("c1", "note:other", "h0")],
                &[],
                None,
                || Ok(()),
            )
            .expect("the unit commits");
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM chunks WHERE source_key = 'note:a'"
            ),
            1,
            "the row landed under the unit's key"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM chunks WHERE source_key = 'note:other'"
            ),
            0,
            "and not under the struct's own claim"
        );
    }

    /// The guard is the transaction's first word: a refusal writes nothing —
    /// not the revision, not the chunks, not the event.
    #[test]
    fn a_guard_refusal_writes_nothing() {
        let (_dir, mut store) = temp_store();
        let result = store.commit_source_file(
            &revision("note:a", "h1"),
            &[chunk("c1", "note:a", "h1")],
            &[edge("note:a", "B")],
            Some(&event("batch", 1, Some("note:a"))),
            || Err(StoreError::GuardRejected("stat moved".to_owned())),
        );
        assert!(
            matches!(result, Err(StoreError::GuardRejected(_))),
            "the refusal surfaces as itself: {result:?}"
        );
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
            0,
            "nothing was written"
        );
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM chunks"), 0);
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM graph_links"), 0);
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM store_events"), 0);
    }

    /// Guard refusal surfaces before any write failure — the guard cannot be
    /// moved after the writes without this test failing.
    #[test]
    fn guard_refusal_surfaces_before_write_failures() {
        let (_dir, mut store) = temp_store();
        store
            .replace_chunks("note:other", "h0", &[chunk("c1", "note:other", "h0")], &[])
            .expect("plant a write failure");
        let result = store.commit_source_file(
            &revision("note:a", "h1"),
            // chunk_id is the primary key: this INSERT fails once the guard
            // has let the unit reach it.
            &[chunk("c1", "note:a", "h1")],
            &[],
            None,
            || Err(StoreError::GuardRejected("stat moved".to_owned())),
        );
        assert!(
            matches!(
                &result,
                Err(StoreError::GuardRejected(msg)) if msg.contains("stat moved")
            ),
            "the guard speaks first, and the refusal carries its own reason \
             (a write failure remapped to this variant would not): {result:?}"
        );
    }

    /// Behavior cannot see a lock mode; the contract says BEGIN IMMEDIATE
    /// (D40 / W2.5) and this needle pins it — the migration runner's shape is
    /// pinned the same way. The needle is split so the test cannot count its
    /// own source text as a hit, and the search is scoped to the function
    /// body so a stray mention elsewhere cannot stand in for the real thing.
    #[test]
    fn the_unit_opens_with_begin_immediate() {
        let file = include_str!("unit.rs");
        let start = file
            .find(concat!("pub fn commit_source", "_file("))
            .expect("the unit exists");
        let body = &file[start..];
        let end = body.find("\n    }").expect("the function body ends");
        let body = &body[..end];
        let needle = concat!("TransactionBehavior", "::Immediate");
        assert!(
            body.contains(needle),
            "the unit opens its transaction with BEGIN IMMEDIATE"
        );
    }

    /// An event that names another source — or names nobody — is refused by
    /// name, and a refused unit writes nothing, chunks and edges included:
    /// riding this unit such a row would claim work that is not here (ADR 0026).
    #[test]
    fn an_event_for_another_source_is_refused() {
        let (_dir, mut store) = temp_store();
        for wrong in [Some("note:b"), None] {
            let result = store.commit_source_file(
                &revision("note:a", "h1"),
                &[chunk("c1", "note:a", "h1")],
                &[edge("note:a", "B")],
                Some(&event("batch", 1, wrong)),
                || Ok(()),
            );
            assert!(
                matches!(
                    result,
                    Err(StoreError::InvalidValue {
                        column: "source_key",
                        ..
                    })
                ),
                "the mismatch is refused by name: {result:?}"
            );
            assert_eq!(
                scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
                0,
                "a refused unit writes nothing"
            );
            assert_eq!(scalar(&store, "SELECT COUNT(*) FROM chunks"), 0);
            assert_eq!(scalar(&store, "SELECT COUNT(*) FROM graph_links"), 0);
            assert_eq!(scalar(&store, "SELECT COUNT(*) FROM store_events"), 0);
        }
    }
}
