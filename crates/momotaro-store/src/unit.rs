//! One file's ingest unit: revision, chunks, edges, and the file's event in a
//! single `BEGIN IMMEDIATE` (D40's unit — the API ADR 0026 registered as
//! follow-up work).

use momotaro_contracts::{Chunk, GraphLink};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::events::NewStoreEvent;
use crate::revisions::NewSourceRevision;
use crate::{Store, StoreError, invalid_value};

impl Store {
    /// Commits one file's ingest unit in exactly one `BEGIN IMMEDIATE`.
    ///
    /// Order inside the transaction is fixed: `guard` runs first (W2.5
    /// re-checks the file's stat there; a refusal rolls the whole unit back),
    /// then the revision upsert, the replacement of the revision's chunks
    /// (rows a reference names are retained — ADR 0022) and of the source's
    /// `graph_links` rows (D40 / ADR 0027), and
    /// finally the file's event when the caller supplies one. Nothing is
    /// written unless the event lands with it, so a crash between data and
    /// event cannot exist inside this unit. A guard that wants its refusal
    /// distinguishable from a write failure returns
    /// [`StoreError::GuardRejected`]; a caller that has no end-of-transaction
    /// check to make passes a no-op, and the index batch is not one of them —
    /// both of its write sites (note and attachment) re-check the file's stat
    /// there, because D40 §4.7's unit is per file rather than per kind.
    ///
    /// Returns whether a revision row was actually *created*. Re-appending a
    /// pair that already exists re-adopts it instead — the shape a deleted file
    /// takes when it comes back — and a caller counting "revisions created"
    /// from this would otherwise claim one for a row that was already there.
    ///
    /// The event must name the revision's source: a row about another source
    /// riding this unit would claim work that is not here (ADR 0026). The data
    /// half replays idempotently (revision upsert; chunks replaced, rows a
    /// reference names retained — ADR 0022 — and edges replaced wholesale);
    /// the event is append-only like every event, and replay
    /// semantics for it stay the caller's (`batch_id` / `seq` / `external_id`).
    ///
    /// The replace's [`ChunkReplaceReport`](crate::chunks::ChunkReplaceReport)
    /// (retained / drift) is returned beside the creation flag, so a drift
    /// signal reaches the ingest caller instead of dying at this wrapper.
    pub fn commit_source_file(
        &mut self,
        revision: &NewSourceRevision<'_>,
        chunks: &[Chunk],
        edges: &[GraphLink],
        event: Option<&NewStoreEvent<'_>>,
        guard: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(bool, crate::chunks::ChunkReplaceReport), StoreError> {
        if let Some(event) = event
            && event.source_key != Some(revision.source_key)
        {
            return Err(invalid_value(
                "source_key",
                event.source_key.unwrap_or("<none>"),
            ));
        }
        // BEGIN IMMEDIATE owns the newest-row read and the append together.
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = crate::events::now_epoch_secs();
        let last: Option<i64> = tx
            .query_row(
                "SELECT recorded_at FROM store_events ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let pending_event = event.map(|event| (event, last.map_or(now, |last| now.max(last))));
        guard()?;
        // Asked inside the transaction, so the answer and the insert
        // cannot disagree about a row someone else wrote in between.
        let prior = tx
            .query_row(
                "SELECT ingested_at FROM source_revisions WHERE source_key = ?1 AND revision_hash = ?2",
                params![revision.source_key, revision.revision_hash],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let existed = prior.is_some();
        // A re-adoption keeps the revision's first stamp (the upsert below
        // never touches `ingested_at`), so the edges carry that same stamp —
        // ADR 0027 replays them from the revision row, and rows holding a
        // later clock would not rebuild byte for byte.
        let restamped;
        let edges = match prior {
            Some(ingested_at) => {
                restamped = edges
                    .iter()
                    .map(|edge| GraphLink {
                        created_at: ingested_at,
                        ..edge.clone()
                    })
                    .collect::<Vec<_>>();
                &restamped[..]
            }
            None => edges,
        };
        crate::revisions::append_revision_tx(&tx, revision)?;
        let report = crate::chunks::replace_chunks_tx(
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
        Ok((!existed, report))
    }

    /// Commits one file's *refresh* in exactly one `BEGIN IMMEDIATE`.
    ///
    /// The sibling of [`Store::commit_source_file`] for the case where the bytes
    /// did not change: the fingerprint, the stored path or the raw spelling moved,
    /// so the mutable columns are rewritten in place and no new revision appears
    /// (`local_path` left behind after a directory move is exactly the staleness
    /// this closes — see the store discipline note on fingerprint refreshes). The
    /// chunks stay as they are — they are addressed by the same
    /// `(source_key, revision_hash)`, so there is nothing to replace — while the
    /// source's `graph_links` rows are re-projected wholesale, because the
    /// namespace they resolve against may have moved since.
    ///
    /// Same order and same guarantees as the sibling: the guard runs first and a
    /// refusal rolls the whole unit back, the event lands with the data or not at
    /// all, and the event must be about this source.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_source_refresh(
        &mut self,
        source_key: &str,
        revision_hash: &str,
        metadata_json: &str,
        local_path: &str,
        raw_name: &str,
        edges: &[GraphLink],
        event: Option<&NewStoreEvent<'_>>,
        guard: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if let Some(event) = event
            && event.source_key != Some(source_key)
        {
            return Err(invalid_value(
                "source_key",
                event.source_key.unwrap_or("<none>"),
            ));
        }
        // BEGIN IMMEDIATE owns the newest-row read and the append together.
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = crate::events::now_epoch_secs();
        let last: Option<i64> = tx
            .query_row(
                "SELECT recorded_at FROM store_events ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let pending_event = event.map(|event| (event, last.map_or(now, |last| now.max(last))));
        guard()?;
        let changed = crate::revisions::update_revision_fingerprint_tx(
            &tx,
            source_key,
            revision_hash,
            metadata_json,
            local_path,
            raw_name,
        )?;
        if changed != 1 {
            return Err(StoreError::RevisionMissing {
                source_key: source_key.to_owned(),
                revision_hash: revision_hash.to_owned(),
            });
        }
        crate::graph::write_edges_tx(&tx, source_key, edges)?;
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
    use crate::test_support::temp_store;
    use momotaro_contracts::{EdgeKind, LinkSource, OriginClass, SourceKind};

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

    /// The shape the replay test below cannot see: the same content returning
    /// under a later clock. The upsert re-adopts the row and keeps its first
    /// `ingested_at`, so the edges must keep that stamp too — ADR 0027 rebuilds
    /// them from the revision row, and rows carrying the later clock would not
    /// replay byte for byte.
    #[test]
    fn a_re_adoption_keeps_the_first_stamp_on_the_edges() {
        let (_dir, mut store) = temp_store();
        let rev = revision("note:a", "h1");
        store
            .commit_source_file(&rev, &[], &[edge("note:a", "B")], None, || Ok(()))
            .expect("first ingest");
        let mut later = revision("note:a", "h1");
        later.ingested_at = 99;
        let mut later_edge = edge("note:a", "B");
        later_edge.created_at = 99;
        store
            .commit_source_file(&later, &[], &[later_edge], None, || Ok(()))
            .expect("re-adoption");
        let rows = store.all_graph_links().expect("read");
        assert_eq!(
            rows[0].created_at, 7,
            "the edges carry the revision's first stamp: {rows:?}"
        );
        assert_eq!(
            store
                .current_revision("note:a")
                .expect("read")
                .expect("row")
                .ingested_at,
            7,
            "and the revision row keeps it as well"
        );
    }

    /// Replaying the unit is idempotent: the same input twice leaves the same
    /// rows — the data half is upsert plus replace (cited chunk rows are
    /// retained, ADR 0022).
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
    /// touches nothing else: the replace's deletes find no rows of this source
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
    /// keyed replace, where they would accumulate.
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

    /// The refresh unit answers the same two questions the build unit does:
    /// the guard is first, and a refusal leaves the row's mutable columns,
    /// its edges and the log exactly as they were (D40 §4.7).
    #[test]
    fn a_guard_refusal_writes_nothing_on_a_refresh() {
        let (_dir, mut store) = temp_store();
        store
            .append_revision(&revision("note:a", "h1"))
            .expect("plant the revision to refresh");
        let before = store
            .current_revision("note:a")
            .expect("read")
            .expect("exists");
        store
            .replace_chunks("note:a", "h1", &[], &[edge("note:a", "B")])
            .expect("plant an edge");

        let result = store.commit_source_refresh(
            "note:a",
            "h1",
            "{}",
            "moved/a.md",
            "a.md",
            &[edge("note:a", "C")],
            Some(&event("batch", 1, Some("note:a"))),
            || Err(StoreError::GuardRejected("stat moved".to_owned())),
        );
        assert!(
            matches!(result, Err(StoreError::GuardRejected(_))),
            "the refusal surfaces as itself: {result:?}"
        );
        let after = store
            .current_revision("note:a")
            .expect("read")
            .expect("exists");
        assert_eq!(
            after.local_path, before.local_path,
            "the path is not moved by a refused refresh"
        );
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM store_events"), 0);
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE raw_target = 'C'"
            ),
            0,
            "the edges the refresh would have written are not there"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE raw_target = 'B'"
            ),
            1,
            "and the old ones are untouched"
        );
    }

    /// What a refresh actually writes, in one unit: the mutable columns, the
    /// re-projected edges and the caller's event — no new revision, because the
    /// bytes did not change (D40 §4.7).
    #[test]
    fn a_refresh_lands_columns_edges_and_event_in_one_unit() {
        let (_dir, mut store) = temp_store();
        store
            .append_revision(&revision("note:a", "h1"))
            .expect("plant the revision");
        store
            .replace_chunks("note:a", "h1", &[], &[edge("note:a", "B")])
            .expect("plant an edge");
        let revisions = scalar(&store, "SELECT COUNT(*) FROM source_revisions");

        store
            .commit_source_refresh(
                "note:a",
                "h1",
                "{\"fingerprint\":1}",
                "moved/a.md",
                "a.md",
                &[edge("note:a", "C")],
                Some(&event("batch", 1, Some("note:a"))),
                || Ok(()),
            )
            .expect("refresh");

        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM source_revisions"),
            revisions,
            "the same bytes append no revision"
        );
        let row = store
            .current_revision("note:a")
            .expect("read")
            .expect("exists");
        assert_eq!(row.local_path.as_deref(), Some("moved/a.md"));
        assert_eq!(row.raw_name.as_deref(), Some("a.md"));
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM store_events"), 1);
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE raw_target = 'C'"
            ),
            1,
            "the re-projection lands with the columns, not after them"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM graph_links WHERE raw_target = 'B'"
            ),
            0,
            "and the old edges are gone wholesale"
        );
    }

    /// Guard refusal surfaces before any write failure — the guard cannot be
    /// moved after the writes without this test failing.
    /// Drift is the out-of-band wipe's one-shot signal. The unit now returns
    /// it beside its creation flag so the ingest layer can turn it into a row.
    #[test]
    fn commit_source_file_returns_chunk_replace_drift() {
        let (_dir, mut store) = temp_store();
        store
            .replace_chunks("note:a", "h1", &[chunk("live", "note:a", "h1")], &[])
            .expect("plant");
        store
            .connection
            .execute(
                "INSERT INTO answers (answer_id, run_id, text, prompt_hash, created_at)
                 VALUES ('answer-1','run-1','text','hash',1)",
                [],
            )
            .expect("plant an answer");
        store
            .connection
            .execute(
                "INSERT INTO answer_citations (
                     answer_id, citation_index, source_key, revision_hash, chunk_id,
                     chunk_hash, locator_json, quoted_text, verified
                 ) VALUES ('answer-1',0,'note:a','h1','live','hash','{}','quote',0)",
                [],
            )
            .expect("plant a citation");
        store
            .connection
            .execute("DELETE FROM chunks WHERE chunk_id = 'live'", [])
            .expect("simulate out-of-band wipe");
        let (_, report) = store
            .commit_source_file(
                &revision("note:a", "h1"),
                &[chunk("new", "note:a", "h1")],
                &[],
                None,
                || Ok(()),
            )
            .expect("commit");
        assert_eq!(report.drift, ["live".to_owned()]);
    }

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
    /// own source text as a hit, and the search is scoped to each function
    /// body so a stray mention elsewhere cannot stand in for the real thing.
    /// Both units are in the search: a needle scoped to `commit_source_file`
    /// alone let the sibling's transaction silently drop to DEFERRED.
    ///
    /// Deliberate extension (W3.7, the run units): the search was widened to
    /// `runs.rs` and its eight `commit_run_*` units when they joined the
    /// family. The family half below enumerates the production halves'
    /// `pub fn commit_*` units and requires a one-to-one match with this
    /// needle's list — so "a new unit function is never invisible to this
    /// needle" is enforced, not merely promised.
    #[test]
    fn the_unit_opens_with_begin_immediate() {
        let needle = concat!("TransactionBehavior", "::Immediate");
        let units: [(&str, &str); 10] = [
            (
                include_str!("unit.rs"),
                concat!("pub fn commit_source", "_file("),
            ),
            (
                include_str!("unit.rs"),
                concat!("pub fn commit_source", "_refresh("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_create("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_start("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_retrieval("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_context("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_llm_turn("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_verifications("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_answer("),
            ),
            (
                include_str!("runs.rs"),
                concat!("pub fn commit_run", "_terminal("),
            ),
        ];
        for (file, entry) in units {
            let start = file.find(entry).expect("the unit exists");
            let body = &file[start..];
            let end = body.find("\n    }").expect("the function body ends");
            let body = &body[..end];
            assert!(
                body.contains(needle),
                "each unit opens its transaction with BEGIN IMMEDIATE ({entry:?})"
            );
        }
        // Family enumeration half: scan the production halves (everything
        // before the cfg(test) module, so this test's own text cannot count)
        // for `pub fn commit_*` and require a one-to-one match with the
        // needle list above — a unit added to the family without a row, or
        // a row without its unit, turns this red.
        let mut family = Vec::new();
        for file in [include_str!("unit.rs"), include_str!("runs.rs")] {
            let production = file
                .split_once("#[cfg(test)]")
                .expect("the test module is cfg-marked")
                .0;
            let mut rest = production;
            while let Some(found) = rest.find("pub fn commit_") {
                rest = &rest[found + "pub fn commit_".len()..];
                let end = rest.find('(').expect("a unit name ends at its paren");
                family.push(rest[..end].to_string());
            }
        }
        family.sort();
        let mut listed: Vec<String> = units
            .iter()
            .map(|(_, entry)| {
                entry
                    .trim_start_matches("pub fn commit_")
                    .trim_end_matches('(')
                    .to_string()
            })
            .collect();
        listed.sort();
        assert_eq!(
            family, listed,
            "the needle list must cover exactly the pub fn commit_* family"
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
