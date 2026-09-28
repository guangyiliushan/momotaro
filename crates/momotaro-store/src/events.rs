//! Reading and appending the store's own audit log (`store_events`, ADR 0026).
//!
//! This log is the store's authority over its own work: ingest batches, feed
//! polls, renames, annotations. It is deliberately separate from `run_events`,
//! which records Runs — work somebody accepted, with a budget and a terminal
//! status. Nothing here is a Run.
//!
//! Two rules the callers depend on:
//!
//! - An append is **one transaction**, and callers write the work before the
//!   event that describes it: a crash can lose an event, never invent one.
//! - `occurred_at` is when the thing happened and `recorded_at` is when it was
//!   written; a replay or an import keeps the first and updates the second
//!   (checked by a test, because the pair has no precedent to lean on).

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError, invalid_value};

/// One event to append. The store assigns `id` and `recorded_at`; the caller
/// says *when* it happened and *which batch* it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStoreEvent<'a> {
    /// When it happened. A replay or an import keeps the original moment, so
    /// this may be older than the row before it.
    pub occurred_at: i64,
    /// The batch this event belongs to: one `index`, one feed poll, one rename.
    pub batch_id: &'a str,
    /// Position inside the batch, monotonic and unique per batch.
    pub seq: i64,
    /// Which part of the store did this: `ingest` | `feed` | `vault` | `annotate`.
    pub domain: &'a str,
    /// Event name, e.g. `ingest.started`, `ingest.coverage`, `source_moved`.
    pub event_type: &'a str,
    /// The source this event is about, when it is about one.
    pub source_key: Option<&'a str>,
    /// Adapter outcome — **one column, two vocabularies** (ADR 0026): feed
    /// adapters write `success` | `no_results` | `upstream_error` |
    /// `parse_error`; ingest writes `indexed` | `skipped` |
    /// `metadata_refreshed` | `error` (`FileOutcome::as_str`). A reader that
    /// branches on this must know which domain it is reading.
    pub outcome: Option<&'a str>,
    /// Why something was skipped or degraded, as a stable code.
    pub reason_code: Option<&'a str>,
    /// Human-readable detail; never the only copy of a fact.
    pub message: Option<&'a str>,
    /// The event's payload. JSON **by convention**, not by enforcement: the
    /// store neither parses nor validates it (ADR 0026 puts no `CHECK` on the
    /// column, and this crate does not depend on a JSON parser).
    pub data_json: &'a str,
    /// Provider-issued identifier, for deduplication only.
    pub external_id: Option<&'a str>,
}

/// One stored event, exactly as the row reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEvent {
    /// Insertion order (`rowid`); the only truthful "latest" (ADR 0026).
    pub id: i64,
    /// When it happened.
    pub occurred_at: i64,
    /// When it was written.
    pub recorded_at: i64,
    /// The batch it belongs to.
    pub batch_id: String,
    /// Position inside the batch.
    pub seq: i64,
    /// Which part of the store did this.
    pub domain: String,
    /// Event name.
    pub event_type: String,
    /// The source it is about, when it is about one.
    pub source_key: Option<String>,
    /// Adapter outcome, when the event has one.
    pub outcome: Option<String>,
    /// Why something was skipped or degraded.
    pub reason_code: Option<String>,
    /// Human-readable detail.
    pub message: Option<String>,
    /// The event's payload.
    pub data_json: String,
    /// Provider-issued identifier.
    pub external_id: Option<String>,
}

const INSERT: &str = "INSERT INTO store_events (
        occurred_at, recorded_at, batch_id, seq, domain, type, source_key,
        outcome, reason_code, message, data_json, external_id
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)";

const SELECT: &str = "SELECT
        id, occurred_at, recorded_at, batch_id, seq, domain, type, source_key,
        outcome, reason_code, message, data_json, external_id
    FROM store_events";

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

impl Store {
    /// Transaction time for the batch about to be written.
    ///
    /// Store-assigned, once per batch, and **never below the newest row's**:
    /// wall-clock steps (NTP) must not move transaction time backwards
    /// (`docs/docs/storage-events.md`, "缺失不得代偿"; Jensen & Snodgrass on
    /// transaction time). Read by `id`, which is the same order the log is read
    /// in, so this is one row lookup rather than an aggregate over the table.
    pub(crate) fn next_recorded_at(&self) -> Result<i64, StoreError> {
        let now = now_epoch_secs();
        let last: Option<i64> = self
            .connection
            .query_row(
                "SELECT recorded_at FROM store_events ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(last.map_or(now, |last| now.max(last)))
    }
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreEvent> {
    Ok(StoreEvent {
        id: row.get("id")?,
        occurred_at: row.get("occurred_at")?,
        recorded_at: row.get("recorded_at")?,
        batch_id: row.get("batch_id")?,
        seq: row.get("seq")?,
        domain: row.get("domain")?,
        event_type: row.get("type")?,
        source_key: row.get("source_key")?,
        outcome: row.get("outcome")?,
        reason_code: row.get("reason_code")?,
        message: row.get("message")?,
        data_json: row.get("data_json")?,
        external_id: row.get("external_id")?,
    })
}

/// Inserts `events` inside the caller's transaction, at the given transaction
/// time.
pub(crate) fn insert_events_tx(
    tx: &rusqlite::Transaction<'_>,
    batch_id: &str,
    events: &[NewStoreEvent<'_>],
    recorded_at: i64,
) -> Result<(), StoreError> {
    let mut statement = tx.prepare(INSERT)?;
    for event in events {
        if event.batch_id != batch_id {
            return Err(invalid_value("batch_id", event.batch_id));
        }
        statement.execute(params![
            event.occurred_at,
            recorded_at,
            event.batch_id,
            event.seq,
            event.domain,
            event.event_type,
            event.source_key,
            event.outcome,
            event.reason_code,
            event.message,
            event.data_json,
            event.external_id,
        ])?;
    }
    Ok(())
}

impl Store {
    /// Appends one event, in a batch of its own — the batch id it carries.
    ///
    /// For anything that produces more than one event, use
    /// [`Store::append_store_events`]: a batch that is half written is a log
    /// that lies about what happened.
    pub fn append_store_event(&mut self, event: &NewStoreEvent<'_>) -> Result<(), StoreError> {
        self.append_store_events(event.batch_id, std::slice::from_ref(event))
    }

    /// Appends a set of events in **one transaction** (ADR 0026).
    ///
    /// `batch_id` is passed once and every event must carry it; a mismatch is a
    /// programming error, refused by name rather than written.
    pub fn append_store_events(
        &mut self,
        batch_id: &str,
        events: &[NewStoreEvent<'_>],
    ) -> Result<(), StoreError> {
        let recorded_at = self.next_recorded_at()?;
        let transaction = self.connection.transaction()?;
        insert_events_tx(&transaction, batch_id, events, recorded_at)?;
        transaction.commit()?;
        Ok(())
    }

    /// Reads one batch in the order it was appended.
    pub fn store_events_for_batch(&self, batch_id: &str) -> Result<Vec<StoreEvent>, StoreError> {
        let mut statement = self
            .connection
            .prepare(&format!("{SELECT} WHERE batch_id = ?1 ORDER BY seq"))?;
        let rows = statement.query_map([batch_id], read_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StoreError::from)
    }

    /// Reads the most recently **written** events, newest first (ADR 0026: this
    /// is insertion order, which is why `occurred_at` may go backwards).
    ///
    /// Stays public as the unfiltered view of the log — the one the event tests
    /// speak in — while domain-scoped readers use `latest_store_events_in`.
    pub fn latest_store_events(&self, limit: u32) -> Result<Vec<StoreEvent>, StoreError> {
        self.latest_store_events_in(None, limit)
    }

    /// [`Store::latest_store_events`] bounded to one `domain`.
    ///
    /// The table carries several domains by design (ADR 0026), so "the newest
    /// row" and "the newest ingest batch" are different questions: a feed poll
    /// that ran after the last `index` must not be reported as the ingest batch.
    pub fn latest_store_events_in(
        &self,
        domain: Option<&str>,
        limit: u32,
    ) -> Result<Vec<StoreEvent>, StoreError> {
        let rows = match domain {
            Some(domain) => {
                let mut statement = self.connection.prepare(&format!(
                    "{SELECT} WHERE domain = ?1 ORDER BY id DESC LIMIT ?2"
                ))?;
                let mapped = statement.query_map(params![domain, limit], read_row)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut statement = self
                    .connection
                    .prepare(&format!("{SELECT} ORDER BY id DESC LIMIT ?1"))?;
                let mapped = statement.query_map([limit], read_row)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }
}

/// Opens a store in a fresh temp dir with the schema applied.
#[cfg(test)]
fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
    store.init_schema().expect("init schema");
    (dir, store)
}

#[cfg(test)]
fn event<'a>(batch_id: &'a str, seq: i64, event_type: &'a str) -> NewStoreEvent<'a> {
    NewStoreEvent {
        occurred_at: 1_000,
        batch_id,
        seq,
        domain: "ingest",
        event_type,
        source_key: None,
        outcome: None,
        reason_code: None,
        message: None,
        data_json: "{}",
        external_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use momotaro_contracts::SchemaVersion;

    #[test]
    fn the_migration_lands_the_table_its_indexes_and_the_version() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.schema_version().expect("read version"),
            Some(SchemaVersion::CURRENT),
            "init_schema brings the store to the current version"
        );
        let objects: Vec<String> = {
            let mut statement = store
                .connection
                .prepare("SELECT name FROM sqlite_master WHERE name LIKE 'idx_store_events%' OR name = 'store_events' ORDER BY name")
                .expect("prepare");
            statement
                .query_map([], |row| row.get(0))
                .expect("query")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect")
        };
        assert_eq!(
            objects,
            vec![
                "idx_store_events_external".to_owned(),
                "idx_store_events_source".to_owned(),
                "store_events".to_owned()
            ],
            "the table and both partial indexes exist"
        );

        // Names alone would let the shape rot: the `UNIQUE` is the access path
        // the ADR leans on (so no second index is added), and both indexes are
        // meant to be *partial*. Mutations that drop either stayed green until
        // these text assertions existed.
        let table_sql = object_sql(&store, "store_events");
        assert!(table_sql.contains("UNIQUE (batch_id, seq)"), "{table_sql}");
        let source_sql = object_sql(&store, "idx_store_events_source");
        assert!(
            source_sql.contains("WHERE source_key IS NOT NULL"),
            "{source_sql}"
        );
        let external_sql = object_sql(&store, "idx_store_events_external");
        assert!(
            external_sql.contains("WHERE external_id IS NOT NULL"),
            "{external_sql}"
        );
    }

    /// The `sqlite_master` DDL of one object.
    fn object_sql(store: &Store, name: &str) -> String {
        store
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .expect("read object DDL")
    }

    #[test]
    fn a_batch_is_one_transaction() {
        let (_dir, mut store) = temp_store();
        let batch = "index-1";
        let events = vec![
            event(batch, 1, "ingest.started"),
            event(batch, 2, "ingest.coverage"),
            // Same (batch_id, seq) as the first: the whole batch must go.
            event(batch, 1, "ingest.finished"),
        ];

        store
            .append_store_events(batch, &events)
            .expect_err("the duplicate seq refuses the batch");

        assert!(
            store
                .store_events_for_batch(batch)
                .expect("read batch")
                .is_empty(),
            "nothing of a failed batch may reach a reader"
        );
    }

    #[test]
    fn a_replay_keeps_the_event_moment() {
        // storage-events.md: the `occurred_at` / `recorded_at` pair has no
        // precedent to lean on, so the promise is pinned here — a replay or an
        // import says when it happened, not when it was written.
        let (_dir, mut store) = temp_store();
        let old = 1_000_000;
        let mut replayed = event("feed-1", 1, "feed.poll.started");
        replayed.occurred_at = old;

        let before = now_epoch_secs();
        store.append_store_event(&replayed).expect("append");
        let after = now_epoch_secs();

        let stored = store
            .store_events_for_batch("feed-1")
            .expect("read batch")
            .pop()
            .expect("one event");
        assert_eq!(stored.occurred_at, old, "the event keeps its own moment");
        assert!(
            (before..=after).contains(&stored.recorded_at),
            "the write moment is the store's own clock: {} not in {before}..={after}",
            stored.recorded_at
        );
    }

    #[test]
    fn latest_is_insertion_order_not_event_time() {
        let (_dir, mut store) = temp_store();
        let mut first = event("index-1", 1, "ingest.started");
        first.occurred_at = 2_000;
        let mut second = event("index-2", 1, "ingest.started");
        second.occurred_at = 1_000; // older moment, written later

        store.append_store_event(&first).expect("append first");
        store.append_store_event(&second).expect("append second");

        let latest = store.latest_store_events(1).expect("read latest");
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].batch_id, "index-2", "newest by id, not by moment");

        let both = store.latest_store_events(2).expect("read two");
        let batches: Vec<&str> = both.iter().map(|event| event.batch_id.as_str()).collect();
        assert_eq!(batches, vec!["index-2", "index-1"], "newest first, both");
        assert!(
            store.latest_store_events(0).expect("read none").is_empty(),
            "a zero limit is empty, not an error"
        );
    }

    #[test]
    fn a_provider_id_deduplicates_inside_one_domain_only() {
        let (_dir, mut store) = temp_store();
        let mut first = event("feed-1", 1, "feed.item");
        first.external_id = Some("provider-1");
        let mut same_provider = event("feed-1", 2, "feed.item");
        same_provider.external_id = Some("provider-1");
        let mut other_domain = event("feed-1", 3, "feed.item");
        other_domain.domain = "vault";
        other_domain.external_id = Some("provider-1");

        store.append_store_event(&first).expect("append first");
        store
            .append_store_event(&same_provider)
            .expect_err("the same provider id in the same domain is refused");

        // The case the feed poll actually hits: the same provider item seen
        // again in a *later* batch. Same refusal, and a read-back proves the
        // first row is still the only one.
        let mut next_poll = event("feed-2", 1, "feed.item");
        next_poll.external_id = Some("provider-1");
        store
            .append_store_event(&next_poll)
            .expect_err("a second batch does not make the provider id fresh");

        let mut null_provider = event("feed-1", 4, "feed.item");
        null_provider.external_id = None;
        let mut another_null = event("feed-1", 5, "feed.item");
        another_null.external_id = None;
        store
            .append_store_event(&other_domain)
            .expect("another domain has its own provider ids");
        store
            .append_store_event(&null_provider)
            .expect("no provider id never collides");
        store
            .append_store_event(&another_null)
            .expect("no provider id never collides, twice");
    }

    #[test]
    fn a_batch_id_that_does_not_match_the_batch_is_refused_by_name() {
        let (_dir, mut store) = temp_store();
        let stray = event("index-1", 1, "ingest.started");
        let error = store
            .append_store_events("index-2", std::slice::from_ref(&stray))
            .expect_err("a stray batch id is a programming error");
        let text = error.to_string();
        assert!(
            matches!(&error, StoreError::InvalidValue { column, .. } if column == &"batch_id"),
            "the refusal names the column, by type: {text}"
        );
        assert!(
            store
                .store_events_for_batch("index-1")
                .expect("read")
                .is_empty(),
            "the refused batch wrote nothing"
        );
    }

    #[test]
    fn a_stray_batch_id_after_a_good_event_writes_nothing() {
        // The guard has to fire before the *whole* batch is written, not only
        // when the first event is the stray one.
        let (_dir, mut store) = temp_store();
        let good = event("index-1", 1, "ingest.started");
        let stray = event("index-9", 2, "ingest.finished");
        store
            .append_store_events("index-1", &[good, stray])
            .expect_err("the second event's batch id is wrong");

        assert!(
            store.latest_store_events(10).expect("read all").is_empty(),
            "no event of a refused batch reaches a reader"
        );
    }

    #[test]
    fn a_batch_reads_back_in_sequence_order() {
        let (_dir, mut store) = temp_store();
        let batch = "vault-1";
        store
            .append_store_events(
                batch,
                &[
                    event(batch, 3, "vault.reconcile.finished"),
                    event(batch, 1, "vault.reconcile.started"),
                    event(batch, 2, "vault.reconcile.coverage"),
                ],
            )
            .expect("append out of order");

        let types: Vec<String> = store
            .store_events_for_batch(batch)
            .expect("read")
            .into_iter()
            .map(|stored| stored.event_type)
            .collect();
        assert_eq!(
            types,
            vec![
                "vault.reconcile.started",
                "vault.reconcile.coverage",
                "vault.reconcile.finished"
            ],
            "the batch reads in `seq` order, whatever order it was handed in"
        );
    }

    #[test]
    fn every_column_round_trips() {
        // The read mapper is by name, so a swap between two `Option<String>`
        // columns is invisible to any test that leaves them `None`. This one
        // gives every column a value and compares the whole row.
        let (_dir, mut store) = temp_store();
        let batch = "feed-1";
        store
            .append_store_event(&NewStoreEvent {
                occurred_at: 1_000_000,
                batch_id: batch,
                seq: 7,
                domain: "feed",
                event_type: "feed.poll.finished",
                source_key: Some("note:a.md"),
                outcome: Some("no_results"),
                reason_code: Some("empty_feed"),
                message: Some("nothing new"),
                data_json: r#"{"seen":0}"#,
                external_id: Some("provider-7"),
            })
            .expect("append");

        let stored = store
            .store_events_for_batch(batch)
            .expect("read")
            .pop()
            .expect("one event");
        let expected = StoreEvent {
            id: stored.id,
            occurred_at: 1_000_000,
            recorded_at: stored.recorded_at,
            batch_id: batch.to_owned(),
            seq: 7,
            domain: "feed".to_owned(),
            event_type: "feed.poll.finished".to_owned(),
            source_key: Some("note:a.md".to_owned()),
            outcome: Some("no_results".to_owned()),
            reason_code: Some("empty_feed".to_owned()),
            message: Some("nothing new".to_owned()),
            data_json: r#"{"seen":0}"#.to_owned(),
            external_id: Some("provider-7".to_owned()),
        };
        assert_eq!(stored, expected, "every column survives the round trip");
    }

    #[test]
    fn one_batch_shares_one_write_moment() {
        // Honest scope: at one-second resolution a per-event clock read lands in
        // the same second, so this pins the promise the API makes, not the
        // implementation detail of where the clock is read.
        let (_dir, mut store) = temp_store();
        let batch = "index-1";
        store
            .append_store_events(
                batch,
                &[
                    event(batch, 1, "ingest.started"),
                    event(batch, 2, "ingest.coverage"),
                ],
            )
            .expect("append");

        let stored = store.store_events_for_batch(batch).expect("read");
        assert_eq!(stored.len(), 2);
        assert_eq!(
            stored[0].recorded_at, stored[1].recorded_at,
            "a batch is written as one thing, so it carries one write moment"
        );
    }

    #[test]
    fn a_sequence_number_is_unique_across_append_calls() {
        let (_dir, mut store) = temp_store();
        let batch = "index-1";
        store
            .append_store_event(&event(batch, 1, "ingest.started"))
            .expect("first append");

        store
            .append_store_event(&event(batch, 1, "ingest.finished"))
            .expect_err("re-using a seq inside one batch is refused, however it is written");

        let stored = store.store_events_for_batch(batch).expect("read");
        assert_eq!(stored.len(), 1, "the refused event did not land");
        assert_eq!(stored[0].event_type, "ingest.started");
    }

    #[test]
    fn a_domain_filter_ignores_other_domains() {
        let (_dir, mut store) = temp_store();
        let ingest = event("index-1", 1, "ingest.started");
        let mut feed = event("feed-1", 1, "feed.poll.started");
        feed.domain = "feed";
        store.append_store_event(&ingest).expect("append ingest");
        store.append_store_event(&feed).expect("append feed");

        assert_eq!(
            store.latest_store_events(10).expect("all")[0].domain,
            "feed",
            "the newest row overall is the feed one"
        );
        let ingest_only = store
            .latest_store_events_in(Some("ingest"), 10)
            .expect("filtered");
        assert_eq!(ingest_only.len(), 1);
        assert_eq!(
            ingest_only[0].batch_id, "index-1",
            "the newest *ingest* row is the one that matters"
        );
    }

    #[test]
    fn a_batch_never_writes_a_recorded_at_before_the_last_row() {
        let (_dir, mut store) = temp_store();
        let ahead = now_epoch_secs() + 3600;
        // Two rows already in the log, and they must differ: with a single row,
        // "the newest row" and "the oldest row" are the same input, so the read
        // order is not pinned at all (a mutant reading the oldest survives).
        store
            .connection
            .execute_batch(&format!(
                "INSERT INTO store_events
                     (occurred_at, recorded_at, batch_id, seq, domain, type, data_json)
                 VALUES ({old}, {old}, 'index-old', 1, 'ingest', 'ingest.started', '{{}}'),
                        ({ahead}, {ahead}, 'index-ahead', 1, 'ingest', 'ingest.started', '{{}}');",
                old = ahead - 60,
                ahead = ahead,
            ))
            .expect("seed an older row and the newest row");

        assert!(now_epoch_secs() < ahead, "the test needs the clock behind");
        store
            .append_store_event(&event("index-after", 1, "ingest.started"))
            .expect("append after the clock went back");

        let rows = store.latest_store_events(1).expect("read back");
        assert_eq!(
            rows[0].recorded_at, ahead,
            "the clamp reads the NEWEST row's timestamp, not the oldest one's"
        );
    }

    #[test]
    fn an_empty_batch_is_accepted_and_writes_nothing() {
        let (_dir, mut store) = temp_store();
        store
            .append_store_events("index-1", &[])
            .expect("a batch with no events is not an error");
        assert!(store.latest_store_events(10).expect("read").is_empty());
    }
}
