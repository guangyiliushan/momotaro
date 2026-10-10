//! Run bookkeeping: the `runs`, `run_events`, `retrievals`, `retrieval_hits`,
//! `run_context_items`, `llm_turns`, `answers`, `answer_citations`,
//! `verifications` and `history_items` tables (`architecture.md` §8.1), and
//! the one-transaction units that write them.
//!
//! The units are the run-side family of [`Store::commit_source_file`]: each
//! lands its rows **and its vocabulary event in the same `BEGIN IMMEDIATE`**
//! — the event-rides-the-transaction shape that closes the lost-event window
//! (`harness.md` §1 for the terminal coupling: `runs.status` is a projection
//! of the terminal event, and the two never exist apart; the intermediate
//! writes get the same shape because a data row whose event is missing is a
//! timeline that lies about what happened).
//!
//! `run_events.seq` is assigned by the store inside each unit's transaction
//! (`MAX(seq) + 1` per run); the `(run_id, seq)` primary key is the backstop
//! (`harness.md` §6). Every row names its own run and the unit cross-checks
//! the parameter against it before anything is written — a row about another
//! run riding this unit would claim work that is not there, the same rule
//! `commit_source_file` enforces for its event. The two names are one fact,
//! so the row's is what lands.

use momotaro_contracts::{
    AnswerCitationRecord, AnswerRecord, ContextItemRecord, HistoryItemRecord, LlmTurnRecord,
    RetrievalHitRecord, RetrievalRecord, RunEventRecord, RunRecord, RunTrace, TerminalEvent,
    VerificationRecord,
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{Store, StoreError, invalid_value};

/// One `run_events` row to append. The store assigns `seq`; the caller says
/// what happened and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunEvent {
    /// Event name from the stable vocabulary (`momotaro_contracts::run_event`).
    pub event_type: String,
    /// The event's payload. JSON by convention; the store does not parse it.
    pub data_json: String,
    /// When the event was written (caller's clock, like `ingested_at`).
    pub created_at: i64,
}

/// The input half of one `runs` row — everything the caller knows at
/// acceptance time. `status` is not here because it is not the caller's to
/// say: creation writes `accepted`, and only terminal events move it
/// (`harness.md` §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    /// Run identity (opaque; the caller generates it).
    pub run_id: String,
    /// Session this run belongs to, when it belongs to one.
    pub session_id: Option<String>,
    /// The run this one is an explicit retry of.
    pub parent_run_id: Option<String>,
    /// Bound project, when there is one.
    pub project_id: Option<String>,
    /// Which attempt this is (retries are new runs).
    pub attempt: i64,
    /// What kind of work this run performs (`RunMode::as_str`).
    pub mode: String,
    /// The asked question, as the user wrote it.
    pub query: String,
    /// Idempotency key: replaying the same request returns the same run
    /// (`harness.md` §1), so this is `UNIQUE` and looked up first.
    pub client_request_id: Option<String>,
    /// Hash of `run_spec_json`'s exact bytes.
    pub run_spec_hash: String,
    /// The frozen RunSpec, byte-stable.
    pub run_spec_json: String,
    /// Provider name recorded at freeze.
    pub provider: Option<String>,
    /// Model id recorded at freeze.
    pub model: Option<String>,
    /// Context window recorded at freeze.
    pub context_window: Option<i64>,
    /// Output ceiling recorded at freeze.
    pub max_output_tokens: Option<i64>,
    /// When the run was accepted.
    pub created_at: i64,
}

/// Whether [`Store::commit_run_create`] created a run or returned the one the
/// `client_request_id` already named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRunOutcome {
    /// A new run row was written.
    Created {
        /// The new run's id.
        run_id: String,
    },
    /// The idempotency key already named a run; nothing was written.
    Existing {
        /// The run the key already named.
        run_id: String,
    },
}

impl CreateRunOutcome {
    /// The run the caller should proceed with (new or replayed).
    pub fn run_id(&self) -> &str {
        match self {
            Self::Created { run_id } | Self::Existing { run_id } => run_id,
        }
    }

    /// Whether this call wrote a new run.
    pub fn created(&self) -> bool {
        matches!(self, Self::Created { .. })
    }
}

/// The event payload of the crash sweep's `run.failed` (`harness.md` §2:
/// `data.category = interrupted`, `attribution = system`,
/// `retryable = true`). A literal: the store neither parses nor validates
/// payload JSON (ADR 0026).
pub(crate) const INTERRUPTED_DATA: &str = "{\"attribution\":\"system\",\"category\":\"interrupted\",\"retryable\":true,\"severity\":\"error\"}";

/// The input half of one `sessions` row (`architecture.md` §8.1).
///
/// `title` is `NOT NULL` in the DDL, so the caller brings one; `ask` derives
/// it from the first query, which is why `title_source` exists — 0.x only
/// ever writes `derived`, and a user's rename is never overwritten by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// Session identity (opaque; the caller generates it).
    pub session_id: String,
    /// Preview title for a session list.
    pub title: String,
    /// How the title was produced (`derived | user | llm`).
    pub title_source: String,
    /// The session's first question, kept whole.
    pub first_user_message: Option<String>,
    /// Project bound at creation; immutable afterward (ADR 0003).
    pub project_id: Option<String>,
    /// When the session was created; also `updated_at` and `last_activity_at`.
    pub created_at: i64,
}

/// One row of `sessions`, as [`Store::sessions`] reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Session identity (opaque; the caller generated it).
    pub session_id: String,
    /// Preview title for a session list.
    pub title: String,
    /// How the title was produced (`derived | user | llm`).
    pub title_source: String,
    /// The session's first question, kept whole.
    pub first_user_message: Option<String>,
    /// When the session was created.
    pub created_at: i64,
    /// When the session last took a turn — the recency axis
    /// `architecture.md` documents as distinct from `created_at`.
    pub last_activity_at: Option<i64>,
}

impl Store {
    /// Commits one run's acceptance in exactly one `BEGIN IMMEDIATE`: the
    /// `runs` row and its `run.accepted` event land together or not at all.
    ///
    /// Replaying a request that carries the same `client_request_id` returns
    /// the run that key already created and writes nothing — idempotent by
    /// construction (`harness.md` §1). A run without a key is never deduped.
    pub fn commit_run_create(
        &mut self,
        run: &NewRun,
        event: &NewRunEvent,
    ) -> Result<CreateRunOutcome, StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(key) = run.client_request_id.as_deref() {
            let existing = tx
                .query_row(
                    "SELECT run_id, run_spec_hash FROM runs WHERE client_request_id = ?1",
                    params![key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((run_id, stored_hash)) = existing {
                if stored_hash != run.run_spec_hash {
                    tx.rollback()?;
                    return Err(StoreError::IdempotencyConflict {
                        key: key.to_owned(),
                        expected: stored_hash,
                        actual: run.run_spec_hash.clone(),
                    });
                }
                tx.rollback()?;
                return Ok(CreateRunOutcome::Existing { run_id });
            }
        }
        // `created_at` is transaction time and must not go backwards: an NTP
        // step would let a later run sort before an earlier one, and the round
        // axis orders by `(runs.created_at, run_id)` (`context-retrieval.md`
        // §3). Same clamp `store_events.recorded_at` applies. This reads
        // `MAX(created_at)` by full scan — `runs` gains a row per ask, and the
        // integer primary key `recorded_at` takes its max from is exactly what
        // `runs` does not have.
        let previous: Option<i64> =
            tx.query_row("SELECT MAX(created_at) FROM runs", [], |row| row.get(0))?;
        let created_at = previous.unwrap_or(i64::MIN).max(run.created_at);
        tx.execute(
            "INSERT INTO runs (
                run_id, session_id, parent_run_id, project_id, graph_id, node_id,
                attempt, mode, query, status, client_request_id, run_spec_hash,
                run_spec_json, provider, model, context_window, max_output_tokens,
                result_digest, failure_note, deadline_at, created_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6, ?7, 'accepted', ?8, ?9,
                ?10, ?11, ?12, ?13, ?14, NULL, NULL, NULL, ?15, NULL)",
            params![
                run.run_id,
                run.session_id,
                run.parent_run_id,
                run.project_id,
                run.attempt,
                run.mode,
                run.query,
                run.client_request_id,
                run.run_spec_hash,
                run.run_spec_json,
                run.provider,
                run.model,
                run.context_window,
                run.max_output_tokens,
                created_at,
            ],
        )?;
        insert_event_tx(&tx, &run.run_id, event)?;
        tx.commit()?;
        Ok(CreateRunOutcome::Created {
            run_id: run.run_id.clone(),
        })
    }

    /// Creates one session row. `session_id` is the primary key, so a second
    /// call for the same id is a constraint failure rather than a silently
    /// second session — the caller checks first and refuses.
    pub fn create_session(&mut self, session: &NewSession) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO sessions (
                session_id, parent_session_id, project_id, title, title_source,
                first_user_message, archived, archived_at, pinned, created_at,
                updated_at, last_activity_at
             ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, 0, NULL, 0, ?6, ?6, ?6)",
            params![
                session.session_id,
                session.project_id,
                session.title,
                session.title_source,
                session.first_user_message,
                session.created_at,
            ],
        )?;
        Ok(())
    }

    /// Whether a session with this id exists — the check that turns a typo'd
    /// `--session` into an error instead of a silently new conversation.
    pub fn session_exists(&self, session_id: &str) -> Result<bool, StoreError> {
        let found: Option<i64> = self
            .connection
            .query_row(
                "SELECT 1 FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Every session, oldest first — the read side [`Self::create_session`]
    /// never had.
    ///
    /// `sessions.title` and `sessions.first_user_message` exist to preview a
    /// conversation in a list (`NewSession`'s own field docs), and `ask`
    /// writes both, but until now nothing could read them back: the row was
    /// write-only and an empty title would have passed every test in the
    /// repo. Also the cheapest way to count sessions, which is how a replayed
    /// request proves it minted no second conversation (`harness.md` §1).
    pub fn sessions(&self) -> Result<Vec<SessionRecord>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT session_id, title, title_source, first_user_message, created_at,
                    last_activity_at
             FROM sessions ORDER BY created_at, session_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(SessionRecord {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    title_source: row.get(2)?,
                    first_user_message: row.get(3)?,
                    created_at: row.get(4)?,
                    last_activity_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One session's history in replay order.
    ///
    /// Ordering is `(created_at, run_id, seq)`: `created_at` alone is not a
    /// total order — SQLite returns ties in an undefined sequence — so the
    /// run id breaks them and `seq` orders the rows inside one run
    /// (`context-retrieval.md` §3).
    pub fn history_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<HistoryItemRecord>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{SELECT_HISTORY_ITEM} WHERE session_id = ?1 ORDER BY created_at, run_id, seq"
        ))?;
        let rows = statement
            .query_map(params![session_id], map_history_item)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The session's turn runs, oldest first — the axis `context-retrieval.md`
    /// §3 numbers rounds by.
    ///
    /// Only runs that actually wrote history count. A failed `ask` leaves no
    /// rows, and if it still consumed a round number the recency guard band
    /// would silently protect one real turn instead of two.
    pub fn turn_runs_for_session(&self, session_id: &str) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT runs.run_id FROM runs
             WHERE {TURN_RUNS_PREDICATE}
             ORDER BY runs.created_at, runs.run_id"
        ))?;
        let rows = statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The session's `run_context_items`, for the runs that wrote history —
    /// the same run set [`Self::turn_runs_for_session`] numbers rounds by.
    ///
    /// This is the source of `first_seen_order` (`context-retrieval.md`
    /// §1.1), which is why the predicate has to match the round axis exactly:
    /// two different run sets would give the ledger and the guard band two
    /// different ideas of what a turn is. The SQL cannot carry the round
    /// order — it needs `runs.created_at`, which lives on the other table —
    /// so the caller re-sorts by the round order it already holds:
    /// `turn_runs_for_session` is the contract's key.
    ///
    /// Projects only the five columns that derivation reads, **not** the whole
    /// [`momotaro_contracts::ContextItemRecord`]: this runs once per
    /// `ask --session` turn and `body` is the largest blob in the schema,
    /// so reading it to discard it is work the row set exists to avoid.
    pub fn context_items_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionContextItem>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "{SELECT_SESSION_CONTEXT_ITEM} WHERE run_id IN (
                 SELECT runs.run_id FROM runs WHERE {TURN_RUNS_PREDICATE}
             ) ORDER BY run_id, ordinal"
        ))?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(SessionContextItem {
                    run_id: row.get(0)?,
                    ordinal: row.get(1)?,
                    fragment_id: row.get(2)?,
                    kind: row.get(3)?,
                    source_ref_json: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Commits the `accepted → running` step in one `BEGIN IMMEDIATE`: the
    /// status projection and its `run.started` event land together.
    pub fn commit_run_start(
        &mut self,
        run_id: &str,
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE runs SET status = 'running' WHERE run_id = ?1 AND status = 'accepted'",
            params![run_id],
        )?;
        if changed != 1 {
            return Err(refuse_transition(&tx, run_id)?);
        }
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Appends one timeline event in a transaction of its own.
    ///
    /// For the moments that write no table rows (`retrieval.started`,
    /// `llm.started`): those are pure events, and an event with no data half
    /// has no half-state to close. Everything that lands rows takes the event
    /// on its unit instead.
    pub fn append_run_event(
        &mut self,
        run_id: &str,
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        ensure_run(&tx, run_id)?;
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits one retrieval in one `BEGIN IMMEDIATE`: the `retrievals` row,
    /// its `retrieval_hits`, and the `retrieval.finished` event together.
    pub fn commit_run_retrieval(
        &mut self,
        run_id: &str,
        retrieval: &RetrievalRecord,
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        if retrieval.run_id != run_id {
            return Err(invalid_value("run_id", retrieval.run_id.clone()));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run_id)?;
        tx.execute(
            "INSERT INTO retrievals (
                retrieval_id, run_id, query, normalized_query, strategy_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                retrieval.retrieval_id,
                retrieval.run_id,
                retrieval.query,
                retrieval.normalized_query,
                retrieval.strategy_json,
                retrieval.created_at,
            ],
        )?;
        for hit in &retrieval.hits {
            insert_hit_tx(&tx, &retrieval.retrieval_id, hit)?;
        }
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits one context build in one `BEGIN IMMEDIATE`: the
    /// `run_context_items` rows and the `context.built` event together.
    pub fn commit_run_context(
        &mut self,
        run_id: &str,
        items: &[ContextItemRecord],
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run_id)?;
        for item in items {
            if item.run_id != run_id {
                return Err(invalid_value("run_id", item.run_id.clone()));
            }
            tx.execute(
                "INSERT INTO run_context_items (
                    run_id, ordinal, fragment_id, kind, role, content_kind, body,
                    source_ref_json, token_estimate, body_hash, decision_rationale,
                    assumptions, supersedes
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    item.run_id,
                    item.ordinal,
                    item.fragment_id,
                    item.kind,
                    item.role,
                    item.content_kind,
                    item.body,
                    item.source_ref_json,
                    item.token_estimate,
                    item.body_hash,
                    item.decision_rationale,
                    item.assumptions,
                    item.supersedes,
                ],
            )?;
        }
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits one provider exchange in one `BEGIN IMMEDIATE`: the
    /// `llm_turns` row and the `llm.finished` event together. The turn row
    /// lands for failed exchanges too (status `failed`) — its usage columns
    /// are the record of what the attempt spent.
    pub fn commit_run_llm_turn(
        &mut self,
        run_id: &str,
        turn: &LlmTurnRecord,
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        if turn.run_id != run_id {
            return Err(invalid_value("run_id", turn.run_id.clone()));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run_id)?;
        tx.execute(
            "INSERT INTO llm_turns (
                turn_id, run_id, seq, provider, model, request_hash, status,
                input_tokens, output_tokens, cached_tokens, billable_tokens, slot,
                reasoning_tokens, raw_ref, created_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                ?15, ?16)",
            params![
                turn.turn_id,
                turn.run_id,
                turn.seq,
                turn.provider,
                turn.model,
                turn.request_hash,
                turn.status,
                turn.input_tokens,
                turn.output_tokens,
                turn.cached_tokens,
                turn.billable_tokens,
                turn.slot,
                turn.reasoning_tokens,
                turn.raw_ref,
                turn.created_at,
                turn.finished_at,
            ],
        )?;
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits one verification batch in one `BEGIN IMMEDIATE`: the
    /// `verifications` rows and the `citation.validated` event together.
    pub fn commit_run_verifications(
        &mut self,
        run_id: &str,
        verifications: &[VerificationRecord],
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run_id)?;
        for verification in verifications {
            if verification.run_id != run_id {
                return Err(invalid_value("run_id", verification.run_id.clone()));
            }
            tx.execute(
                "INSERT INTO verifications (
                    verification_id, run_id, answer_id, kind, status, details_json,
                    created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    verification.verification_id,
                    verification.run_id,
                    verification.answer_id,
                    verification.kind,
                    verification.status,
                    verification.details_json,
                    verification.created_at,
                ],
            )?;
        }
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits one answer in one `BEGIN IMMEDIATE`: the `answers` row, its
    /// `answer_citations`, the `history_items` turns this run appends, and the
    /// `answer.finalized` event together. An answer never lands without its
    /// citations (an uncited answer is a legitimate state — a *half* answer
    /// is not: the two would be indistinguishable).
    pub fn commit_run_answer(
        &mut self,
        run_id: &str,
        answer: &AnswerRecord,
        history: &[HistoryItemRecord],
        event: &NewRunEvent,
    ) -> Result<(), StoreError> {
        if answer.run_id != run_id {
            return Err(invalid_value("run_id", answer.run_id.clone()));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_run(&tx, run_id)?;
        // The run's session is the key `turn_runs_for_session` numbers rounds
        // by; `history_items.session_id` is the key `history_for_session`
        // replays them by. A row written with the other one splits the two —
        // the round axis counts a turn the replay cannot see, so a session
        // gains a phantom round and the guard band protects one turn fewer
        // than exist. `idx_history_items_session` is why that column is read
        // directly rather than joined off `runs`, so the agreement has to be
        // enforced where the row is written (`context-retrieval.md` §3).
        let run_session: Option<String> = tx.query_row(
            "SELECT session_id FROM runs WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO answers (
                answer_id, run_id, text, prompt_hash, created_at, decision_rationale,
                assumptions, supersedes
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                answer.answer_id,
                answer.run_id,
                answer.text,
                answer.prompt_hash,
                answer.created_at,
                answer.decision_rationale,
                answer.assumptions,
                answer.supersedes,
            ],
        )?;
        for citation in &answer.citations {
            tx.execute(
                "INSERT INTO answer_citations (
                    answer_id, citation_index, source_key, revision_hash, chunk_id,
                    chunk_hash, locator_json, quoted_text, verified
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    answer.answer_id,
                    citation.citation_index,
                    citation.source_key,
                    citation.revision_hash,
                    citation.chunk_id,
                    citation.chunk_hash,
                    citation.locator_json,
                    citation.quoted_text,
                    citation.verified as i64,
                ],
            )?;
        }
        // `history_items.created_at` is transaction time and has to move the
        // way `runs.created_at` does. Replay order is
        // `(created_at, run_id, seq)` while the round axis is
        // `(runs.created_at, runs.run_id)`; clamp only the round axis and a
        // clock step between the two writes replays the conversation
        // **backwards** while the rounds stay monotone — and
        // `guard_band_from_round` accumulates its 8000-token bound over
        // whichever sequence it was handed (`context-retrieval.md` §3).
        // Both sequences are then non-decreasing in insertion order and ties
        // fall to `run_id`, so the two axes cannot come apart. Same clamp
        // `runs.created_at` and `store_events.recorded_at` already apply.
        let mut history_clock: i64 = tx
            .query_row("SELECT MAX(created_at) FROM history_items", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?
            .unwrap_or(i64::MIN);
        for item in history {
            if item.run_id != run_id {
                return Err(invalid_value("run_id", item.run_id.clone()));
            }
            if item.session_id != run_session {
                return Err(invalid_value(
                    "session_id",
                    item.session_id.clone().unwrap_or_else(|| "NULL".to_owned()),
                ));
            }
            history_clock = history_clock.max(item.created_at);
            let created_at = history_clock;
            tx.execute(
                "INSERT INTO history_items (
                    history_id, session_id, run_id, seq, kind, role, content_kind,
                    body, payload_json, source_ref_json, token_estimate, created_at,
                    occurred_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    item.history_id,
                    item.session_id,
                    item.run_id,
                    item.seq,
                    item.kind,
                    item.role,
                    item.content_kind,
                    item.body,
                    item.payload_json,
                    item.source_ref_json,
                    item.token_estimate,
                    created_at,
                    item.occurred_at,
                ],
            )?;
        }
        // `sessions.last_activity_at` is documented as「recency 排序，区别于
        // created_at」(`architecture.md`) — a column set once at insert and
        // never again would contradict that comment from the second turn on.
        // Bumped inside the same unit as the history it describes, and
        // clamped to its own stored value so a clock step back cannot make a
        // session look older than it is (the same rule `runs.created_at` and
        // `history_items.created_at` already follow).
        if let Some(session) = run_session.as_ref() {
            let seen: Option<i64> = tx
                .query_row(
                    "SELECT last_activity_at FROM sessions WHERE session_id = ?1",
                    params![session],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let activity = seen.unwrap_or(i64::MIN).max(answer.created_at);
            tx.execute(
                "UPDATE sessions
                    SET last_activity_at = ?1, updated_at = ?1
                  WHERE session_id = ?2",
                params![activity, session],
            )?;
        }
        insert_event_tx(&tx, run_id, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Commits one run's terminal step in one `BEGIN IMMEDIATE` — the
    /// coupling `harness.md` §1 makes non-negotiable: the terminal event and
    /// the status update it projects land atomically, so
    /// 「status=finished 但没有 terminal 事件」and its mirror cannot exist.
    ///
    /// The status is **derived from the event** (one spelling source:
    /// [`TerminalEvent::projected_status`]), not passed separately. A run
    /// that is already terminal — or missing — is refused and nothing lands.
    /// `failure_note` annotates a failed run's partial output (multi-agent
    /// §5.1: 失败必标注部分产出).
    pub fn commit_run_terminal(
        &mut self,
        run_id: &str,
        terminal: TerminalEvent,
        data_json: &str,
        failure_note: Option<&str>,
        finished_at: i64,
    ) -> Result<(), StoreError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let event = NewRunEvent {
            event_type: terminal.as_str().to_owned(),
            data_json: data_json.to_owned(),
            created_at: finished_at,
        };
        insert_event_tx(&tx, run_id, &event)?;
        let changed = tx.execute(
            "UPDATE runs SET status = ?2, finished_at = ?3, failure_note = ?4
             WHERE run_id = ?1 AND status IN ('accepted', 'running')",
            params![
                run_id,
                terminal.projected_status(),
                finished_at,
                failure_note
            ],
        )?;
        if changed != 1 {
            return Err(refuse_transition(&tx, run_id)?);
        }
        tx.commit()?;
        Ok(())
    }

    /// How many runs this store holds, in any status.
    ///
    /// The witness for a *read-only* command. [`Self::sweep_interrupted_runs`]
    /// only ever sees `accepted` / `running` rows, so a finished run — exactly
    /// what a successful one leaves behind — is invisible to it; asking whether
    /// `--mode expand` or a refused `ask` created a run needs the terminal
    /// count, not the sweep.
    pub fn run_count(&self) -> Result<u64, StoreError> {
        // Same shape as `count_batch`: SQLite counts come back signed.
        let count = self
            .connection
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get::<_, i64>(0))?;
        Ok(u64::try_from(count).unwrap_or(u64::MAX))
    }

    /// The deterministic crash sweep (`harness.md` §2): every run left
    /// `accepted` or `running` with no terminal event is closed as
    /// `failed(interrupted)` — the terminal unit per run, so an interrupted
    /// sweep is itself resumable. Returns the swept run ids.
    ///
    /// Callers: the run engine's store opens (`ask` / `trace`). Read-only
    /// inspectors do not sweep — `doctor` writes nothing (`usage.md`), so
    /// "the run engine's open sweeps" is where the rule is implemented.
    ///
    /// The「属于已死进程」half of `harness.md` §2 is **not** a third clause in
    /// this predicate — §2 now says so itself. It is implied by 0.x's
    /// single-writer discipline (harness §5), which also records that
    /// discipline's known breach: two concurrent CLIs across sessions sweep
    /// each other's *live* runs. Not evaluated here; a live run cannot
    /// coexist with this sweep **in one** process. A future desktop that
    /// opens a second writer mid-run owes the clause a real liveness check
    /// before that happens.
    pub fn sweep_interrupted_runs(&mut self) -> Result<Vec<String>, StoreError> {
        let pending: Vec<String> = self
            .connection
            .prepare(&format!(
                "SELECT run_id FROM runs WHERE {ACTIVE_RUN_PREDICATE}"
            ))?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        let mut swept = Vec::with_capacity(pending.len());
        for run_id in pending {
            // The note names the last step that completed, so a swept run's
            // partial output is annotated rather than swallowed
            // (multi-agent §5.1). `none` when the timeline is empty.
            let last: Option<String> = self
                .connection
                .query_row(
                    "SELECT type FROM run_events WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
                    params![run_id],
                    |row| row.get(0),
                )
                .optional()?;
            let note = match last {
                Some(last) => format!("interrupted after {last}"),
                None => "interrupted before any step".to_owned(),
            };
            let now = crate::events::now_epoch_secs();
            self.commit_run_terminal(
                &run_id,
                TerminalEvent::Failed,
                INTERRUPTED_DATA,
                Some(&note),
                now,
            )?;
            swept.push(run_id);
        }
        Ok(swept)
    }

    /// Reads one run row, when it exists.
    fn run_record(&self, run_id: &str) -> Result<Option<RunRecord>, StoreError> {
        let mut statement = self
            .connection
            .prepare(&format!("{SELECT_RUN} WHERE run_id = ?1"))?;
        let record = statement.query_row(params![run_id], map_run).optional()?;
        Ok(record)
    }

    /// Reads the run one idempotency key names, when one exists.
    pub fn run_id_for_client_request(
        &self,
        client_request_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let run_id = self
            .connection
            .query_row(
                "SELECT run_id FROM runs WHERE client_request_id = ?1",
                params![client_request_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(run_id)
    }

    /// Reads the run id and frozen spec hash one idempotency key names.
    ///
    /// Replay is equal to the *same request*, not merely the same key: ask
    /// compares the caller's freshly frozen hash to the stored one before it
    /// replays.
    pub fn run_request_hash_for_client_request(
        &self,
        client_request_id: &str,
    ) -> Result<Option<(String, String)>, StoreError> {
        self.connection
            .query_row(
                "SELECT run_id, run_spec_hash FROM runs WHERE client_request_id = ?1",
                params![client_request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Reads everything one run wrote, in replay order (`harness.md` §6.2:
    /// `run_events` + canonical tables rebuild the complete timeline).
    pub fn run_trace(&self, run_id: &str) -> Result<Option<RunTrace>, StoreError> {
        let Some(run) = self.run_record(run_id)? else {
            return Ok(None);
        };

        let mut statement = self.connection.prepare(
            "SELECT seq, type, data_json, created_at FROM run_events
             WHERE run_id = ?1 ORDER BY seq",
        )?;
        let events = statement
            .query_map(params![run_id], |row| {
                Ok(RunEventRecord {
                    seq: row.get(0)?,
                    event_type: row.get(1)?,
                    data_json: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;

        let mut retrievals = Vec::new();
        let mut statement = self.connection.prepare(
            "SELECT retrieval_id, query, normalized_query, strategy_json, created_at
             FROM retrievals WHERE run_id = ?1 ORDER BY created_at, retrieval_id",
        )?;
        let heads = statement
            .query_map(params![run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (retrieval_id, query, normalized_query, strategy_json, created_at) in heads {
            let mut statement = self.connection.prepare(&format!(
                "{SELECT_HIT} WHERE retrieval_id = ?1 ORDER BY rank"
            ))?;
            let hits = statement
                .query_map(params![retrieval_id], map_hit)?
                .collect::<Result<Vec<_>, _>>()?;
            retrievals.push(RetrievalRecord {
                retrieval_id,
                run_id: run_id.to_owned(),
                query,
                normalized_query,
                strategy_json,
                created_at,
                hits,
            });
        }

        let mut statement = self.connection.prepare(&format!(
            "{SELECT_CONTEXT_ITEM} WHERE run_id = ?1 ORDER BY ordinal"
        ))?;
        let context_items = statement
            .query_map(params![run_id], map_context_item)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut statement = self
            .connection
            .prepare(&format!("{SELECT_LLM_TURN} WHERE run_id = ?1 ORDER BY seq"))?;
        let llm_turns = statement
            .query_map(params![run_id], map_llm_turn)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut statement = self.connection.prepare(&format!(
            "{SELECT_VERIFICATION} WHERE run_id = ?1 ORDER BY created_at, verification_id"
        ))?;
        let verifications = statement
            .query_map(params![run_id], map_verification)?
            .collect::<Result<Vec<_>, _>>()?;

        let answer = {
            let mut statement = self
                .connection
                .prepare(&format!("{SELECT_ANSWER} WHERE run_id = ?1"))?;
            let mut answers = statement
                .query_map(params![run_id], map_answer)?
                .collect::<Result<Vec<_>, _>>()?;
            if answers.len() > 1 {
                return Err(StoreError::Invariant(
                    "one run wrote more than one answer row",
                ));
            }
            answers.pop()
        };
        let answer = match answer {
            Some(mut answer) => {
                let mut statement = self.connection.prepare(&format!(
                    "{SELECT_CITATION} WHERE answer_id = ?1 ORDER BY citation_index"
                ))?;
                answer.citations = statement
                    .query_map(params![answer.answer_id], map_citation)?
                    .collect::<Result<Vec<_>, _>>()?;
                Some(answer)
            }
            None => None,
        };

        let mut statement = self.connection.prepare(&format!(
            "{SELECT_HISTORY_ITEM} WHERE run_id = ?1 ORDER BY seq"
        ))?;
        let history = statement
            .query_map(params![run_id], map_history_item)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(RunTrace {
            run,
            events,
            retrievals,
            context_items,
            llm_turns,
            verifications,
            answer,
            history,
        }))
    }
}

const SELECT_RUN: &str = "SELECT
        run_id, session_id, parent_run_id, project_id, graph_id, node_id, attempt,
        mode, query, status, client_request_id, run_spec_hash, run_spec_json,
        provider, model, context_window, max_output_tokens, result_digest,
        failure_note, deadline_at, created_at, finished_at
    FROM runs";

const SELECT_HIT: &str = "SELECT
        rank, source_key, revision_hash, chunk_id, chunk_hash, locator_json, score,
        included, omitted_reason
    FROM retrieval_hits";

const SELECT_CONTEXT_ITEM: &str = "SELECT
        run_id, ordinal, fragment_id, kind, role, content_kind, body,
        source_ref_json, token_estimate, body_hash, decision_rationale, assumptions,
        supersedes
    FROM run_context_items";

/// The projection [`Store::context_items_for_session`] uses: the five columns
/// `derive_session_state` reads, and nothing else.
///
/// Deliberately not `SELECT_CONTEXT_ITEM`. That reader runs once per
/// `ask --session` turn over a session's whole history of context rows, and
/// `body` is the largest blob in the schema — while `role`, `content_kind`,
/// `token_estimate`, `body_hash`, `decision_rationale`, `assumptions` and
/// `supersedes` are never looked at there at all. Projecting them to discard
/// them would be invisible to this crate's own tests, which assert on
/// `run_id`.
const SELECT_SESSION_CONTEXT_ITEM: &str = "SELECT
        run_id, ordinal, fragment_id, kind, source_ref_json
    FROM run_context_items";

/// One `run_context_items` row as [`Store::context_items_for_session`] reads
/// it — the ledger derivation's five columns, and no more.
///
/// Not [`momotaro_contracts::ContextItemRecord`]: that type is the full row
/// `run_trace` replays, which is a different job with a different cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContextItem {
    /// The run that wrote the fragment (the round axis's key).
    pub run_id: String,
    /// Position inside that run's build.
    pub ordinal: i64,
    /// `f{NNNN}` id, parsed numerically for `fragment_seq`.
    pub fragment_id: String,
    /// `evidence` is the only kind that earns a position slot.
    pub kind: String,
    /// The fragment's citation payload — where `chunk_hash` lives.
    pub source_ref_json: Option<String>,
}

const SELECT_LLM_TURN: &str = "SELECT
        turn_id, run_id, seq, provider, model, request_hash, status, input_tokens,
        output_tokens, cached_tokens, billable_tokens, slot, reasoning_tokens,
        raw_ref, created_at, finished_at
    FROM llm_turns";

const SELECT_VERIFICATION: &str = "SELECT
        verification_id, run_id, answer_id, kind, status, details_json, created_at
    FROM verifications";

const SELECT_ANSWER: &str = "SELECT
        answer_id, run_id, text, prompt_hash, created_at, decision_rationale,
        assumptions, supersedes
    FROM answers";

const SELECT_CITATION: &str = "SELECT
        citation_index, source_key, revision_hash, chunk_id, chunk_hash,
        locator_json, quoted_text, verified
    FROM answer_citations";

const SELECT_HISTORY_ITEM: &str = "SELECT
        history_id, session_id, run_id, seq, kind, role, content_kind, body,
        payload_json, source_ref_json, token_estimate, created_at, occurred_at
    FROM history_items";

/// "This run is still open": a pre-terminal status and no terminal event.
///
/// Read by exactly one query, [`Store::sweep_interrupted_runs`] — which is
/// also what makes it a *dead-run* predicate rather than a liveness test: a
/// row matching it has no terminal event, which in 0.x's single-writer
/// discipline (`harness.md` §5) means its process is gone, since the sweep
/// itself runs in the open that would have to coexist with it. `ask` used to
/// ask the same predicate as a "session busy" refusal before it swept; that
/// refusal could only ever fire on a run about to be reaped, so it now lets
/// the sweep go first instead. Adding a terminal event type means changing
/// this one string.
const ACTIVE_RUN_PREDICATE: &str = "status IN ('accepted', 'running')
            AND NOT EXISTS (
              SELECT 1 FROM run_events
              WHERE run_events.run_id = runs.run_id
                AND type IN ('run.finished', 'run.failed', 'run.cancelled')
            )";

/// The runs a session's turns are numbered over: the ones that wrote
/// history, nothing else.
///
/// Shared by [`Store::turn_runs_for_session`] (which supplies the round
/// axis) and [`Store::context_items_for_session`] (which supplies the
/// position ledger). Two spellings would give the ledger and the guard band
/// different ideas of what a turn is — and only one direction of that
/// mismatch is loud, because a stray item would sort last rather than error.
///
/// `context-retrieval.md` §3 defines a round as an **ask** run, and this
/// predicate does not say `runs.mode = 'ask'`. That is equivalent in 0.x and
/// only because `mode = search` runs are created with `session_id = NULL`
/// (`index_service.rs`), so they cannot match the first clause at all; the
/// day another mode starts writing session-scoped history the two stop being
/// the same set. Adding the clause now would mean adding it to
/// [`Store::history_for_session`] too — which reads `history_items`, a table
/// with no `mode` column, so it would need a subquery and give up the
/// left-most-column use of `idx_history_items_session` that schema v8 exists
/// for. Registered as the latent gap it is rather than paid for today.
const TURN_RUNS_PREDICATE: &str = "runs.session_id = ?1
               AND EXISTS (
                 SELECT 1 FROM history_items
                 WHERE history_items.run_id = runs.run_id
               )";

/// Inserts one event at the store's next `seq` for the run. Caller supplies
/// the transaction: this is how the event rides the data's unit.
fn insert_event_tx(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    event: &NewRunEvent,
) -> Result<(), StoreError> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = ?1",
        params![run_id],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO run_events (run_id, seq, type, data_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            run_id,
            seq,
            event.event_type,
            event.data_json,
            event.created_at
        ],
    )?;
    Ok(())
}

fn insert_hit_tx(
    tx: &rusqlite::Transaction<'_>,
    retrieval_id: &str,
    hit: &RetrievalHitRecord,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO retrieval_hits (
            retrieval_id, rank, source_key, revision_hash, chunk_id, chunk_hash,
            locator_json, score, included, omitted_reason
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            retrieval_id,
            hit.rank,
            hit.source_key,
            hit.revision_hash,
            hit.chunk_id,
            hit.chunk_hash,
            hit.locator_json,
            hit.score,
            hit.included as i64,
            hit.omitted_reason,
        ],
    )?;
    Ok(())
}

/// The run must exist for a write to land on it. Asked inside the
/// transaction, so the answer and the write cannot disagree about a row
/// someone else wrote in between.
fn ensure_run(tx: &rusqlite::Transaction<'_>, run_id: &str) -> Result<(), StoreError> {
    let found: Option<String> = tx
        .query_row(
            "SELECT run_id FROM runs WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
        .optional()?;
    found.map(|_| ()).ok_or_else(|| StoreError::RunMissing {
        run_id: run_id.to_owned(),
    })
}

/// Why a status transition found nothing to move: the run is missing, or it
/// is already terminal. Distinguished by name so the caller reports the
/// state instead of a bare "0 rows".
fn refuse_transition(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
) -> Result<StoreError, StoreError> {
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM runs WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match status {
        None => StoreError::RunMissing {
            run_id: run_id.to_owned(),
        },
        Some(status) => invalid_value("status", status),
    })
}

fn map_run(row: &rusqlite::Row<'_>) -> Result<RunRecord, rusqlite::Error> {
    Ok(RunRecord {
        run_id: row.get(0)?,
        session_id: row.get(1)?,
        parent_run_id: row.get(2)?,
        project_id: row.get(3)?,
        graph_id: row.get(4)?,
        node_id: row.get(5)?,
        attempt: row.get(6)?,
        mode: row.get(7)?,
        query: row.get(8)?,
        status: row.get(9)?,
        client_request_id: row.get(10)?,
        run_spec_hash: row.get(11)?,
        run_spec_json: row.get(12)?,
        provider: row.get(13)?,
        model: row.get(14)?,
        context_window: row.get(15)?,
        max_output_tokens: row.get(16)?,
        result_digest: row.get(17)?,
        failure_note: row.get(18)?,
        deadline_at: row.get(19)?,
        created_at: row.get(20)?,
        finished_at: row.get(21)?,
    })
}

fn map_hit(row: &rusqlite::Row<'_>) -> Result<RetrievalHitRecord, rusqlite::Error> {
    Ok(RetrievalHitRecord {
        rank: row.get(0)?,
        source_key: row.get(1)?,
        revision_hash: row.get(2)?,
        chunk_id: row.get(3)?,
        chunk_hash: row.get(4)?,
        locator_json: row.get(5)?,
        score: row.get(6)?,
        included: row.get::<_, i64>(7)? != 0,
        omitted_reason: row.get(8)?,
    })
}

fn map_context_item(row: &rusqlite::Row<'_>) -> Result<ContextItemRecord, rusqlite::Error> {
    Ok(ContextItemRecord {
        run_id: row.get(0)?,
        ordinal: row.get(1)?,
        fragment_id: row.get(2)?,
        kind: row.get(3)?,
        role: row.get(4)?,
        content_kind: row.get(5)?,
        body: row.get(6)?,
        source_ref_json: row.get(7)?,
        token_estimate: row.get(8)?,
        body_hash: row.get(9)?,
        decision_rationale: row.get(10)?,
        assumptions: row.get(11)?,
        supersedes: row.get(12)?,
    })
}

fn map_llm_turn(row: &rusqlite::Row<'_>) -> Result<LlmTurnRecord, rusqlite::Error> {
    Ok(LlmTurnRecord {
        turn_id: row.get(0)?,
        run_id: row.get(1)?,
        seq: row.get(2)?,
        provider: row.get(3)?,
        model: row.get(4)?,
        request_hash: row.get(5)?,
        status: row.get(6)?,
        input_tokens: row.get(7)?,
        output_tokens: row.get(8)?,
        cached_tokens: row.get(9)?,
        billable_tokens: row.get(10)?,
        slot: row.get(11)?,
        reasoning_tokens: row.get(12)?,
        raw_ref: row.get(13)?,
        created_at: row.get(14)?,
        finished_at: row.get(15)?,
    })
}

fn map_verification(row: &rusqlite::Row<'_>) -> Result<VerificationRecord, rusqlite::Error> {
    Ok(VerificationRecord {
        verification_id: row.get(0)?,
        run_id: row.get(1)?,
        answer_id: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        details_json: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn map_answer(row: &rusqlite::Row<'_>) -> Result<AnswerRecord, rusqlite::Error> {
    Ok(AnswerRecord {
        answer_id: row.get(0)?,
        run_id: row.get(1)?,
        text: row.get(2)?,
        prompt_hash: row.get(3)?,
        created_at: row.get(4)?,
        decision_rationale: row.get(5)?,
        assumptions: row.get(6)?,
        supersedes: row.get(7)?,
        citations: Vec::new(),
    })
}

fn map_citation(row: &rusqlite::Row<'_>) -> Result<AnswerCitationRecord, rusqlite::Error> {
    Ok(AnswerCitationRecord {
        citation_index: row.get(0)?,
        source_key: row.get(1)?,
        revision_hash: row.get(2)?,
        chunk_id: row.get(3)?,
        chunk_hash: row.get(4)?,
        locator_json: row.get(5)?,
        quoted_text: row.get(6)?,
        verified: row.get::<_, i64>(7)? != 0,
    })
}

fn map_history_item(row: &rusqlite::Row<'_>) -> Result<HistoryItemRecord, rusqlite::Error> {
    Ok(HistoryItemRecord {
        history_id: row.get(0)?,
        session_id: row.get(1)?,
        run_id: row.get(2)?,
        seq: row.get(3)?,
        kind: row.get(4)?,
        role: row.get(5)?,
        content_kind: row.get(6)?,
        body: row.get(7)?,
        payload_json: row.get(8)?,
        source_ref_json: row.get(9)?,
        token_estimate: row.get(10)?,
        created_at: row.get(11)?,
        occurred_at: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_store;
    use momotaro_contracts::{ModelSlot, RunMode};

    fn new_run(run_id: &str) -> NewRun {
        NewRun {
            run_id: run_id.to_owned(),
            session_id: Some("sess-1".to_owned()),
            parent_run_id: None,
            project_id: None,
            attempt: 1,
            mode: RunMode::Ask.as_str().to_owned(),
            query: "why is the sky blue?".to_owned(),
            client_request_id: None,
            run_spec_hash: "hash".to_owned(),
            run_spec_json: "{}".to_owned(),
            provider: Some("fake".to_owned()),
            model: Some("fake-model".to_owned()),
            context_window: Some(32_768),
            max_output_tokens: Some(4096),
            created_at: 100,
        }
    }

    fn event(event_type: &str, at: i64) -> NewRunEvent {
        NewRunEvent {
            event_type: event_type.to_owned(),
            data_json: "{}".to_owned(),
            created_at: at,
        }
    }

    fn create(store: &mut Store, run_id: &str) -> CreateRunOutcome {
        store
            .commit_run_create(
                &new_run(run_id),
                &event(momotaro_contracts::run_event::ACCEPTED, 100),
            )
            .expect("create run")
    }

    /// `NewRun` is what a caller writes and `RunRecord` is what it reads back:
    /// two hand-written listings of one row. Every field the caller supplies
    /// has to survive the trip, or the INSERT and the SELECT have drifted —
    /// the row is written one way and the reader is handed another.
    #[test]
    fn every_field_a_caller_writes_survives_the_trip_back() {
        let (_dir, mut store) = temp_store();
        assert!(create(&mut store, "run-1").created());
        let record = store
            .run_record("run-1")
            .expect("read it back")
            .expect("the row is there");
        let expected = new_run("run-1");
        assert_eq!(record.run_id, expected.run_id);
        assert_eq!(record.session_id, expected.session_id);
        assert_eq!(record.parent_run_id, expected.parent_run_id);
        assert_eq!(record.project_id, expected.project_id);
        assert_eq!(record.attempt, expected.attempt);
        assert_eq!(record.mode, expected.mode);
        assert_eq!(record.query, expected.query);
        assert_eq!(record.client_request_id, expected.client_request_id);
        assert_eq!(record.run_spec_hash, expected.run_spec_hash);
        assert_eq!(record.run_spec_json, expected.run_spec_json);
        assert_eq!(record.provider, expected.provider);
        assert_eq!(record.model, expected.model);
        assert_eq!(record.context_window, expected.context_window);
        assert_eq!(record.max_output_tokens, expected.max_output_tokens);
        assert_eq!(record.created_at, expected.created_at);
    }

    fn context_item(run_id: &str, ordinal: i64) -> ContextItemRecord {
        ContextItemRecord {
            run_id: run_id.to_owned(),
            ordinal,
            fragment_id: format!("f{ordinal:04}"),
            kind: "evidence".to_owned(),
            role: "user".to_owned(),
            content_kind: "text".to_owned(),
            body: "body".to_owned(),
            source_ref_json: Some(
                "{\"source_key\":\"note:a.md\",\"revision_hash\":\"h1\",\
                 \"chunk_id\":\"c1\",\"chunk_hash\":\"ch1\",\
                 \"locator_json\":\"{}\",\"origin_class\":\"owner\"}"
                    .to_owned(),
            ),
            token_estimate: 1,
            body_hash: "bh".to_owned(),
            decision_rationale: None,
            assumptions: None,
            supersedes: None,
        }
    }

    fn scalar(store: &Store, sql: &str) -> i64 {
        store
            .connection
            .query_row(sql, [], |row| row.get(0))
            .expect("scalar")
    }

    /// The lost-event window cannot open on a run unit: when the event cannot
    /// land, the rows do not survive it. The trigger makes the event insert
    /// fail — the one failure shape the API cannot otherwise reach. Three
    /// units carry the behavioral witness (create, context, terminal); the
    /// rest share one event-landing path (`insert_event_tx`) and are held by
    /// the structural needles (one `BEGIN IMMEDIATE` per unit, every write
    /// site through its unit).
    #[test]
    fn run_count_sees_the_rows_the_sweep_cannot() {
        // `sweep_interrupted_runs` only ever looks at `accepted` / `running`,
        // so a finished run — what a *successful* one leaves — is invisible to
        // it. `run_count` exists for exactly that case, and this pins both the
        // count and the difference.
        let dir = tempfile::tempdir().expect("temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open");
        store.init_schema().expect("init");
        assert_eq!(store.run_count().expect("count"), 0, "an empty store");

        create(&mut store, "run-1");
        assert_eq!(store.run_count().expect("count"), 1, "a live run counts");

        store
            .connection
            .execute(
                "UPDATE runs SET status = 'finished' WHERE run_id = 'run-1'",
                [],
            )
            .expect("finish the run");
        assert_eq!(
            store.run_count().expect("count"),
            1,
            "a finished run counts too"
        );
        assert!(
            store.sweep_interrupted_runs().expect("sweep").is_empty(),
            "and the sweep still cannot see it — which is why count is the witness"
        );
    }

    #[test]
    fn an_event_that_cannot_land_takes_the_rows_with_it() {
        let (_dir, mut store) = temp_store();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_event BEFORE INSERT ON run_events
                 BEGIN SELECT RAISE(ABORT, 'event refused'); END;",
            )
            .expect("plant the trigger");

        // create: the run row must not survive its missing acceptance event.
        let result = store.commit_run_create(
            &new_run("run-1"),
            &event(momotaro_contracts::run_event::ACCEPTED, 100),
        );
        assert!(result.is_err(), "the event insert must fail: {result:?}");
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM runs"),
            0,
            "the run rolled back with its event"
        );

        // context: the fragment rows must not survive their missing event.
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_event")
            .expect("drop trigger");
        create(&mut store, "run-1");
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_event BEFORE INSERT ON run_events
                 BEGIN SELECT RAISE(ABORT, 'event refused'); END;",
            )
            .expect("plant the trigger again");
        let result = store.commit_run_context(
            "run-1",
            &[context_item("run-1", 0)],
            &event(momotaro_contracts::run_event::CONTEXT_BUILT, 200),
        );
        assert!(result.is_err(), "the event insert must fail: {result:?}");
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM run_context_items"),
            0,
            "the rows rolled back with their event"
        );
    }

    /// The terminal coupling, spelled the way `harness.md` §1 demands: the
    /// status is a projection of the terminal event and the two never exist
    /// apart. This test carries the direction "the event fails — the status
    /// stays"; the mirror ("a status can never claim a terminal whose event
    /// did not land") rides `a_second_terminal_event_is_refused` and
    /// `a_terminal_event_for_a_missing_run_is_refused`.
    #[test]
    fn the_terminal_event_and_the_status_share_one_fate() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");

        // A terminal event whose insert fails leaves the status where it was.
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_event BEFORE INSERT ON run_events
                 BEGIN SELECT RAISE(ABORT, 'event refused'); END;",
            )
            .expect("plant the trigger");
        let result = store.commit_run_terminal("run-1", TerminalEvent::Failed, "{}", None, 200);
        assert!(result.is_err(), "the event insert must fail: {result:?}");
        let status: String = store
            .connection
            .query_row("SELECT status FROM runs WHERE run_id = 'run-1'", [], |r| {
                r.get(0)
            })
            .expect("run row survives");
        assert_eq!(status, "accepted", "the status was not projected alone");

        // And the projection lands with the event.
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_event")
            .expect("drop trigger");
        store
            .commit_run_terminal("run-1", TerminalEvent::Finished, "{}", None, 200)
            .expect("terminal lands");
        let trace = store.run_trace("run-1").expect("read").expect("run exists");
        assert_eq!(trace.run.status, "finished");
        assert_eq!(trace.run.finished_at, Some(200));
        assert_eq!(
            trace.events.last().expect("event").event_type,
            "run.finished"
        );
    }

    /// A run that is already terminal refuses a second terminal event — the
    /// mirror half of "no terminal status without a terminal event": no
    /// terminal event without one status.
    #[test]
    fn a_second_terminal_event_is_refused() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        store
            .commit_run_terminal("run-1", TerminalEvent::Cancelled, "{}", None, 200)
            .expect("first terminal");
        let result = store.commit_run_terminal("run-1", TerminalEvent::Finished, "{}", None, 300);
        assert!(
            matches!(
                result,
                Err(StoreError::InvalidValue {
                    column: "status",
                    ..
                })
            ),
            "a terminal run keeps its first fate: {result:?}"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM run_events WHERE type LIKE 'run.%'"
            ),
            2,
            "run.accepted + the first terminal, and nothing more"
        );
    }

    /// A terminal for a run that does not exist names the run, writes nothing.
    #[test]
    fn a_terminal_event_for_a_missing_run_is_refused() {
        let (_dir, mut store) = temp_store();
        let result = store.commit_run_terminal("ghost", TerminalEvent::Failed, "{}", None, 200);
        assert!(
            matches!(result, Err(StoreError::RunMissing { .. })),
            "{result:?}"
        );
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM run_events"), 0);
    }

    /// Replaying a request with the same `client_request_id` returns the run
    /// that key already created (`harness.md` §8.2): same run, no second row,
    /// no second acceptance event.
    #[test]
    fn replaying_a_client_request_returns_the_same_run() {
        let (_dir, mut store) = temp_store();
        let mut first = new_run("run-1");
        first.client_request_id = Some("req-1".to_owned());
        let outcome = store
            .commit_run_create(&first, &event(momotaro_contracts::run_event::ACCEPTED, 100))
            .expect("create");
        assert!(outcome.created());

        let mut replay = new_run("run-2");
        replay.client_request_id = Some("req-1".to_owned());
        replay.created_at = 999;
        let outcome = store
            .commit_run_create(
                &replay,
                &event(momotaro_contracts::run_event::ACCEPTED, 999),
            )
            .expect("replay");
        assert!(!outcome.created(), "the replay creates nothing");
        assert_eq!(outcome.run_id(), "run-1", "and returns the original run");
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM runs"), 1);
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM run_events"), 1);
    }

    /// The same key for a different frozen request is a conflict, not a
    /// replay. The stored hash is the request identity.
    #[test]
    fn a_reused_client_request_with_a_different_hash_conflicts() {
        let (_dir, mut store) = temp_store();
        let mut first = new_run("run-1");
        first.client_request_id = Some("req-1".to_owned());
        store
            .commit_run_create(&first, &event(momotaro_contracts::run_event::ACCEPTED, 100))
            .expect("create");

        let mut replay = new_run("run-2");
        replay.client_request_id = Some("req-1".to_owned());
        replay.run_spec_hash = "other".to_owned();
        let result = store.commit_run_create(
            &replay,
            &event(momotaro_contracts::run_event::ACCEPTED, 101),
        );
        assert!(
            matches!(&result, Err(StoreError::IdempotencyConflict { key, .. }) if key == "req-1"),
            "{result:?}"
        );
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM runs"), 1);
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM run_events"), 1);
    }

    /// A session binds its project when the row is created (ADR 0003); NULL is
    /// the unbound state, never a deferred decision.
    #[test]
    fn a_session_stores_the_project_bound_at_creation() {
        let (_dir, mut store) = temp_store();
        store
            .create_session(&NewSession {
                session_id: "sess-1".to_owned(),
                title: "title".to_owned(),
                title_source: "derived".to_owned(),
                first_user_message: None,
                project_id: Some("project-1".to_owned()),
                created_at: 10,
            })
            .expect("create");
        let stored: Option<String> = store
            .connection
            .query_row(
                "SELECT project_id FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .expect("read");
        assert_eq!(stored.as_deref(), Some("project-1"));
    }

    /// A citation is a five-tuple written with the answer, not an enforced FK
    /// to the retained chunks projection. This pins the intentional write-time
    /// semantics before anyone adds an FK and silently changes them.
    #[test]
    fn an_answer_can_record_a_dangling_citation() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        let mut answer = answer_of("run-1");
        answer.citations.clear();
        answer
            .citations
            .push(momotaro_contracts::AnswerCitationRecord {
                citation_index: 0,
                source_key: "note:a.md".to_owned(),
                revision_hash: "h1".to_owned(),
                chunk_id: "missing".to_owned(),
                chunk_hash: "ch".to_owned(),
                locator_json: "{}".to_owned(),
                quoted_text: "quote".to_owned(),
                verified: false,
            });
        store
            .commit_run_answer(
                "run-1",
                &answer,
                &[turn_row("hist-1", "run-1", 1, 100)],
                &event(momotaro_contracts::run_event::ANSWER_FINALIZED, 120),
            )
            .expect("write");
        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM answer_citations WHERE chunk_id = 'missing'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1);
    }

    /// Without a key there is nothing to dedupe on: two requests, two runs.
    #[test]
    fn two_requests_without_a_key_are_two_runs() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        create(&mut store, "run-2");
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM runs"), 2);
    }

    /// The sweep closes exactly the dangling: `accepted` or `running` with no
    /// terminal event, annotated with the last step that completed
    /// (`harness.md` §2 / multi-agent §5.1). Terminal runs are untouched, and
    /// a second sweep finds nothing.
    #[test]
    fn the_sweep_closes_dangling_runs_as_interrupted() {
        let (_dir, mut store) = temp_store();
        // Dangling at `accepted` (never started).
        create(&mut store, "run-accepted");
        // Dangling at `running` (started, never finished).
        create(&mut store, "run-running");
        store
            .commit_run_start(
                "run-running",
                &event(momotaro_contracts::run_event::STARTED, 110),
            )
            .expect("start");
        // Already terminal: not dangling.
        create(&mut store, "run-done");
        store
            .commit_run_terminal("run-done", TerminalEvent::Finished, "{}", None, 120)
            .expect("finish");

        let swept = store.sweep_interrupted_runs().expect("sweep");
        assert_eq!(swept.len(), 2, "both dangling runs were swept: {swept:?}");

        let trace = store
            .run_trace("run-running")
            .expect("read")
            .expect("exists");
        assert_eq!(trace.run.status, "failed");
        let last = trace.events.last().expect("terminal event");
        assert_eq!(last.event_type, "run.failed");
        assert_eq!(
            last.data_json, INTERRUPTED_DATA,
            "category/attribution/retryable"
        );
        assert_eq!(
            trace.run.failure_note.as_deref(),
            Some("interrupted after run.started"),
            "the note names the last completed step"
        );

        let accepted = store
            .run_trace("run-accepted")
            .expect("read")
            .expect("exists");
        assert_eq!(
            accepted.run.failure_note.as_deref(),
            Some("interrupted after run.accepted")
        );

        let done = store.run_trace("run-done").expect("read").expect("exists");
        assert_eq!(done.run.status, "finished", "terminal runs keep their fate");

        assert!(
            store
                .sweep_interrupted_runs()
                .expect("second sweep")
                .is_empty(),
            "a second sweep finds nothing to do"
        );
    }

    /// The interrupted payload is the harness section-2 shape plus the
    /// section-18 severity — every spelling in it traced to a contract enum
    /// or a contract sentence, none of them floating.
    #[test]
    fn the_interrupted_payload_pins_every_spelling_to_its_source() {
        use momotaro_contracts::Severity;
        assert_eq!(
            INTERRUPTED_DATA,
            format!(
                "{{\"attribution\":\"system\",\"category\":\"interrupted\",\"retryable\":true,\"severity\":\"{}\"}}",
                Severity::Error.as_str()
            )
        );
    }

    /// The sweep's terminal-event clause is load-bearing: a run whose
    /// timeline already carries a terminal event is never swept, even when
    /// its status projection says otherwise. The half-state is unreachable
    /// through the units (status and event land together) and is planted
    /// here by raw SQL — drift any of the three type literals in the sweep's
    /// `NOT EXISTS` and this goes red (all three are planted).
    #[test]
    fn a_terminal_event_on_the_timeline_wins_over_a_stale_status() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        store
            .connection
            .execute_batch(
                "INSERT INTO run_events (run_id, seq, type, data_json, created_at)
                 VALUES ('run-1', 99, 'run.finished', '{}', 200);
                 UPDATE runs SET status = 'running' WHERE run_id = 'run-1';",
            )
            .expect("plant the half-state");
        // The same half-state under each of the three terminal spellings:
        // one planted literal would pin one arm of the sweep's `NOT EXISTS`
        // and leave the other two free to drift.
        for (run_id, terminal) in [("run-2", "run.failed"), ("run-3", "run.cancelled")] {
            create(&mut store, run_id);
            let sql = format!(
                "INSERT INTO run_events (run_id, seq, type, data_json, created_at) VALUES ('{run_id}', 99, '{terminal}', '{{}}', 200); UPDATE runs SET status = 'running' WHERE run_id = '{run_id}';"
            );
            store
                .connection
                .execute_batch(&sql)
                .expect("plant the half-state");
        }
        let swept = store.sweep_interrupted_runs().expect("sweep");
        assert!(
            swept.is_empty(),
            "the terminal events own their runs: {swept:?}"
        );
        assert_eq!(
            scalar(
                &store,
                "SELECT COUNT(*) FROM run_events WHERE type = 'run.failed' AND seq = 99"
            ),
            1,
            "and no interrupted event was invented for the planted ones"
        );
    }

    /// A run with no timeline at all (the accepted row written, its event
    /// lost to a pre-unit crash shape) still gets an honest note.
    #[test]
    fn a_swept_run_with_an_empty_timeline_says_so() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        store
            .connection
            .execute("DELETE FROM run_events WHERE run_id = 'run-1'", [])
            .expect("erase the timeline (a state the API cannot write)");
        store.sweep_interrupted_runs().expect("sweep");
        let trace = store.run_trace("run-1").expect("read").expect("exists");
        assert_eq!(
            trace.run.failure_note.as_deref(),
            Some("interrupted before any step")
        );
    }

    /// `seq` climbs per run and per run only — the store assigns the next one
    /// inside each unit (`harness.md` §6.1).
    #[test]
    fn event_seqs_climb_per_run() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-a");
        create(&mut store, "run-b");
        store
            .commit_run_start("run-a", &event(momotaro_contracts::run_event::STARTED, 1))
            .expect("start a");
        store
            .commit_run_start("run-b", &event(momotaro_contracts::run_event::STARTED, 1))
            .expect("start b");
        store
            .append_run_event(
                "run-a",
                &event(momotaro_contracts::run_event::RETRIEVAL_STARTED, 2),
            )
            .expect("append");
        let a = store.run_trace("run-a").expect("read").expect("exists");
        let b = store.run_trace("run-b").expect("read").expect("exists");
        let seqs = |trace: &RunTrace| trace.events.iter().map(|e| e.seq).collect::<Vec<_>>();
        assert_eq!(seqs(&a), vec![1, 2, 3], "run-a's timeline: {a:?}");
        assert_eq!(seqs(&b), vec![1, 2], "run-b's timeline: {b:?}");
    }

    /// One write naming another run rides nothing: the row claiming work that
    /// is not there is refused by name (the `commit_source_file` event rule).
    #[test]
    fn a_row_naming_another_run_is_refused() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        let mut item = context_item("run-other", 0);
        item.run_id = "run-other".to_owned();
        let result = store.commit_run_context(
            "run-1",
            &[item],
            &event(momotaro_contracts::run_event::CONTEXT_BUILT, 200),
        );
        assert!(
            matches!(
                result,
                Err(StoreError::InvalidValue {
                    column: "run_id",
                    ..
                })
            ),
            "{result:?}"
        );
        assert_eq!(scalar(&store, "SELECT COUNT(*) FROM run_context_items"), 0);
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM run_events"),
            1,
            "the refusal wrote no event either — only run.accepted is there"
        );
    }

    /// The store owns the turn order key: two turns of one run cannot share a
    /// `seq`, or `run_trace`'s `ORDER BY seq` replay order would fall to
    /// SQLite's unspecified scan order (`idx_llm_turns_run_seq`, schema v9).
    #[test]
    fn a_duplicate_llm_turn_seq_is_refused() {
        let (_dir, mut store) = temp_store();
        create(&mut store, "run-1");
        let turn = LlmTurnRecord {
            turn_id: "turn-1".to_owned(),
            run_id: "run-1".to_owned(),
            seq: 1,
            provider: "fake".to_owned(),
            model: "fake-model".to_owned(),
            request_hash: "rh".to_owned(),
            status: "finished".to_owned(),
            input_tokens: Some(3),
            output_tokens: Some(5),
            cached_tokens: Some(1),
            billable_tokens: Some(2),
            slot: Some(ModelSlot::Answer.as_str().to_owned()),
            reasoning_tokens: Some(0),
            raw_ref: Some("chatcmpl-1".to_owned()),
            created_at: 140,
            finished_at: Some(141),
        };
        store
            .commit_run_llm_turn(
                "run-1",
                &turn,
                &event(momotaro_contracts::run_event::LLM_FINISHED, 141),
            )
            .expect("the first turn of the run");
        let replay = LlmTurnRecord {
            turn_id: "turn-2".to_owned(),
            ..turn
        };
        assert!(
            store
                .commit_run_llm_turn(
                    "run-1",
                    &replay,
                    &event(momotaro_contracts::run_event::LLM_FINISHED, 142),
                )
                .is_err(),
            "a second turn with the same (run_id, seq) is refused"
        );
    }
    /// Everything one run wrote, read back field for field: the write side
    /// and the read side must not be two truths (a column swap on either side
    /// goes red here).
    #[test]
    fn the_trace_round_trips_every_row() {
        let (_dir, mut store) = temp_store();
        let mut run = new_run("run-1");
        run.client_request_id = Some("req-1".to_owned());
        store
            .commit_run_create(&run, &event(momotaro_contracts::run_event::ACCEPTED, 100))
            .expect("create");
        store
            .commit_run_start("run-1", &event(momotaro_contracts::run_event::STARTED, 110))
            .expect("start");

        let retrieval = RetrievalRecord {
            retrieval_id: "ret-1".to_owned(),
            run_id: "run-1".to_owned(),
            query: "sky".to_owned(),
            normalized_query: "sky".to_owned(),
            strategy_json: "{\"k\":10}".to_owned(),
            created_at: 120,
            hits: vec![
                RetrievalHitRecord {
                    rank: 1,
                    source_key: "note:a.md".to_owned(),
                    revision_hash: "h1".to_owned(),
                    chunk_id: "c1".to_owned(),
                    chunk_hash: "ch1".to_owned(),
                    locator_json: "{\"start_line\":1}".to_owned(),
                    score: Some(2.5),
                    included: true,
                    omitted_reason: None,
                },
                RetrievalHitRecord {
                    rank: 2,
                    source_key: "note:b.md".to_owned(),
                    revision_hash: "h2".to_owned(),
                    chunk_id: "c2".to_owned(),
                    chunk_hash: "ch2".to_owned(),
                    locator_json: "{\"start_line\":9}".to_owned(),
                    score: None,
                    included: false,
                    omitted_reason: Some("budget_exhausted".to_owned()),
                },
            ],
        };
        store
            .commit_run_retrieval(
                "run-1",
                &retrieval,
                &event(momotaro_contracts::run_event::RETRIEVAL_FINISHED, 121),
            )
            .expect("retrieval");

        let items = vec![
            context_item("run-1", 0),
            ContextItemRecord {
                decision_rationale: Some("why".to_owned()),
                assumptions: Some("what".to_owned()),
                supersedes: Some("f0000".to_owned()),
                ..context_item("run-1", 1)
            },
        ];
        store
            .commit_run_context(
                "run-1",
                &items,
                &event(momotaro_contracts::run_event::CONTEXT_BUILT, 130),
            )
            .expect("context");

        let turn = LlmTurnRecord {
            turn_id: "turn-1".to_owned(),
            run_id: "run-1".to_owned(),
            seq: 1,
            provider: "fake".to_owned(),
            model: "fake-model".to_owned(),
            request_hash: "rh".to_owned(),
            status: "finished".to_owned(),
            input_tokens: Some(3),
            output_tokens: Some(5),
            cached_tokens: Some(1),
            billable_tokens: Some(2),
            slot: Some(ModelSlot::Answer.as_str().to_owned()),
            reasoning_tokens: Some(0),
            raw_ref: Some("chatcmpl-1".to_owned()),
            created_at: 140,
            finished_at: Some(141),
        };
        store
            .commit_run_llm_turn(
                "run-1",
                &turn,
                &event(momotaro_contracts::run_event::LLM_FINISHED, 141),
            )
            .expect("turn");

        let verification = VerificationRecord {
            verification_id: "ver-1".to_owned(),
            run_id: "run-1".to_owned(),
            answer_id: Some("ans-1".to_owned()),
            kind: "citation".to_owned(),
            status: "passed".to_owned(),
            details_json: "{}".to_owned(),
            created_at: 150,
        };
        store
            .commit_run_verifications(
                "run-1",
                std::slice::from_ref(&verification),
                &event(momotaro_contracts::run_event::CITATION_VALIDATED, 150),
            )
            .expect("verifications");

        let answer = AnswerRecord {
            answer_id: "ans-1".to_owned(),
            run_id: "run-1".to_owned(),
            text: "the sky is Rayleigh-scattered [f0001: sky is blue]".to_owned(),
            prompt_hash: "ph".to_owned(),
            created_at: 160,
            decision_rationale: None,
            assumptions: None,
            supersedes: None,
            citations: vec![AnswerCitationRecord {
                citation_index: 0,
                source_key: "note:a.md".to_owned(),
                revision_hash: "h1".to_owned(),
                chunk_id: "c1".to_owned(),
                chunk_hash: "ch1".to_owned(),
                locator_json: "{\"start_line\":1}".to_owned(),
                quoted_text: "sky is blue".to_owned(),
                verified: true,
            }],
        };
        let history = vec![HistoryItemRecord {
            history_id: "hist-1".to_owned(),
            session_id: Some("sess-1".to_owned()),
            run_id: "run-1".to_owned(),
            seq: 1,
            kind: "turn".to_owned(),
            role: "assistant".to_owned(),
            content_kind: "text".to_owned(),
            body: answer.text.clone(),
            payload_json: "{\"referenced_chunk_hashes\":[\"ch1\"]}".to_owned(),
            source_ref_json: None,
            token_estimate: 12,
            created_at: 160,
            occurred_at: Some(160),
        }];
        store
            .commit_run_answer(
                "run-1",
                &answer,
                &history,
                &event(momotaro_contracts::run_event::ANSWER_FINALIZED, 160),
            )
            .expect("answer");
        store
            .commit_run_terminal("run-1", TerminalEvent::Finished, "{}", None, 170)
            .expect("terminal");

        let trace = store.run_trace("run-1").expect("read").expect("exists");

        // The run row, field for field (the freeze round-trips byte-stable).
        let expected_run = RunRecord {
            run_id: "run-1".to_owned(),
            session_id: Some("sess-1".to_owned()),
            parent_run_id: None,
            project_id: None,
            graph_id: None,
            node_id: None,
            attempt: 1,
            mode: "ask".to_owned(),
            query: "why is the sky blue?".to_owned(),
            status: "finished".to_owned(),
            client_request_id: Some("req-1".to_owned()),
            run_spec_hash: "hash".to_owned(),
            run_spec_json: "{}".to_owned(),
            provider: Some("fake".to_owned()),
            model: Some("fake-model".to_owned()),
            context_window: Some(32_768),
            max_output_tokens: Some(4096),
            result_digest: None,
            failure_note: None,
            deadline_at: None,
            created_at: 100,
            finished_at: Some(170),
        };
        assert_eq!(trace.run, expected_run);

        assert_eq!(
            trace.retrievals,
            vec![RetrievalRecord {
                run_id: "run-1".to_owned(),
                ..retrieval
            }]
        );
        assert_eq!(trace.context_items, items);
        assert_eq!(trace.llm_turns, vec![turn]);
        assert_eq!(trace.verifications, vec![verification]);
        let round = trace.answer.expect("answer row");
        assert_eq!(round, answer);
        assert_eq!(trace.history, history);

        // The timeline carries every vocabulary event the run wrote, in order.
        let names = trace
            .events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "run.accepted",
                "run.started",
                "retrieval.finished",
                "context.built",
                "llm.finished",
                "citation.validated",
                "answer.finalized",
                "run.finished",
            ]
        );
    }

    /// The idempotency lookup is its own read: the key answers the run id.
    #[test]
    fn the_client_request_key_resolves_to_its_run() {
        let (_dir, mut store) = temp_store();
        let mut run = new_run("run-1");
        run.client_request_id = Some("req-1".to_owned());
        store
            .commit_run_create(&run, &event(momotaro_contracts::run_event::ACCEPTED, 100))
            .expect("create");
        assert_eq!(
            store.run_id_for_client_request("req-1").expect("read"),
            Some("run-1".to_owned())
        );
        assert_eq!(
            store.run_id_for_client_request("req-2").expect("read"),
            None
        );
        assert_eq!(
            store.run_trace("ghost").expect("read"),
            None,
            "a trace for a run that does not exist is None, not an error"
        );
    }

    /// A run with a caller-chosen session and creation time.
    fn run_at(run_id: &str, session_id: Option<&str>, created_at: i64) -> NewRun {
        NewRun {
            created_at,
            session_id: session_id.map(str::to_owned),
            ..new_run(run_id)
        }
    }

    fn accepted(store: &mut Store, run: &NewRun) {
        store
            .commit_run_create(
                run,
                &event(momotaro_contracts::run_event::ACCEPTED, run.created_at),
            )
            .expect("create run");
    }

    fn answer_of(run_id: &str) -> AnswerRecord {
        AnswerRecord {
            answer_id: format!("ans-{run_id}"),
            run_id: run_id.to_owned(),
            text: "the answer".to_owned(),
            prompt_hash: "ph".to_owned(),
            created_at: 160,
            decision_rationale: None,
            assumptions: None,
            supersedes: None,
            citations: Vec::new(),
        }
    }

    fn turn_row(history_id: &str, run_id: &str, seq: i64, created_at: i64) -> HistoryItemRecord {
        HistoryItemRecord {
            history_id: history_id.to_owned(),
            session_id: Some("sess-1".to_owned()),
            run_id: run_id.to_owned(),
            seq,
            kind: "turn".to_owned(),
            role: "user".to_owned(),
            content_kind: "text".to_owned(),
            body: format!("body of {history_id}"),
            payload_json: "{\"referenced_chunk_hashes\":[]}".to_owned(),
            source_ref_json: None,
            token_estimate: 4,
            created_at,
            occurred_at: Some(created_at),
        }
    }

    /// Landing history for a run is what makes it a *turn* run.
    ///
    /// The row's `session_id` comes from the run rather than being spelled
    /// here: `commit_run_answer` refuses a row that disagrees with it, and
    /// that refusal is the witness for the round axis and the replay reading
    /// the same session.
    fn append_turns(store: &mut Store, run_id: &str, seqs: &[i64], created_at: i64) {
        let session_id: Option<String> = store
            .connection
            .query_row(
                "SELECT session_id FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .expect("the run exists");
        let history: Vec<HistoryItemRecord> = seqs
            .iter()
            .map(|seq| {
                let mut row = turn_row(&format!("hist-{run_id}-{seq}"), run_id, *seq, created_at);
                row.session_id = session_id.clone();
                row
            })
            .collect();
        store
            .commit_run_answer(
                run_id,
                &answer_of(run_id),
                &history,
                &event(momotaro_contracts::run_event::ANSWER_FINALIZED, created_at),
            )
            .expect("answer");
    }

    fn created_at_of(store: &Store, run_id: &str) -> i64 {
        store
            .connection
            .query_row(
                "SELECT created_at FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .expect("read created_at")
    }

    /// Two rows whose `created_at` differ, written in the wrong order: the
    /// clamp must bring the late one forward. Removing the clamp, or reading
    /// `MIN` instead of `MAX`, both leave it behind — one assertion kills
    /// both, which is why the fixture seeds **two** rows on the sort key.
    #[test]
    fn a_run_never_lands_a_created_at_behind_what_the_table_already_holds() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-earlier", None, 1_000));
        accepted(&mut store, &run_at("run-later-by-clock", None, 500));

        assert_eq!(
            created_at_of(&store, "run-earlier"),
            1_000,
            "the first run keeps the time it was accepted at"
        );
        assert_eq!(
            created_at_of(&store, "run-later-by-clock"),
            1_000,
            "a clock that stepped backwards must not sort a later run first \
             (context-retrieval.md §3 orders rounds by (created_at, run_id))"
        );
    }

    /// History reads back `(created_at, run_id, seq)`: `created_at` across
    /// runs, `seq` inside one run — and the fixture writes `seq` **out of
    /// order**, so a reader without `ORDER BY` returns them inverted.
    #[test]
    fn history_reads_back_created_at_then_run_then_seq() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-bbb", Some("sess-1"), 100));
        accepted(&mut store, &run_at("run-aaa", Some("sess-1"), 100));
        accepted(&mut store, &run_at("run-ccc", Some("sess-1"), 200));
        append_turns(&mut store, "run-bbb", &[2, 1], 100);
        append_turns(&mut store, "run-aaa", &[1], 100);
        append_turns(&mut store, "run-ccc", &[1], 200);

        let rows = store.history_for_session("sess-1").expect("read history");
        let ids: Vec<&str> = rows.iter().map(|r| r.history_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "hist-run-aaa-1",
                "hist-run-bbb-1",
                "hist-run-bbb-2",
                "hist-run-ccc-1",
            ],
            "created_at first (200 last), then run_id inside the tie, then seq \
             inside the run — `run-bbb`'s rows went in as 2,1 and come back 1,2"
        );
        assert!(
            store
                .history_for_session("no-such-session")
                .expect("read")
                .is_empty(),
            "an unknown session reads as empty, not an error"
        );
    }

    /// The tie-break is a SQL-level promise, not something a fixture can
    /// force: SQLite leaves rows tied on every sort key in an *undefined*
    /// sequence, so no fixture can make "drop `run_id` from the ORDER BY"
    /// fail deterministically. The clause itself is what carries it, so the
    /// clause is what gets pinned — needles are `concat!`-built and the
    /// search skips the `#[cfg(test)]` half, so this test cannot satisfy
    /// itself.
    #[test]
    fn the_history_reader_pins_its_tie_break_clause() {
        let source = include_str!("runs.rs");
        let production = source
            .split(concat!("#[cfg(", "test)]"))
            .next()
            .expect("the source has a start");
        let needle = concat!("WHERE session_id = ?1 ", "ORDER BY created_at, run_id, seq");
        assert!(
            production.contains(needle),
            "history_for_session must order by (created_at, run_id, seq) — \
             SQLite does not define the order of a full tie"
        );
    }

    /// Only runs that actually wrote history are turns: a failed `ask` leaves
    /// no rows and must not consume a round number, or the recency guard band
    /// silently protects one real turn instead of two.
    #[test]
    fn only_runs_that_wrote_history_count_as_turns() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-1", Some("sess-1"), 100));
        accepted(&mut store, &run_at("run-2-failed", Some("sess-1"), 200));
        accepted(&mut store, &run_at("run-3", Some("sess-1"), 300));
        append_turns(&mut store, "run-1", &[1], 100);
        append_turns(&mut store, "run-3", &[1], 300);

        assert_eq!(
            store.turn_runs_for_session("sess-1").expect("turns"),
            vec!["run-1".to_owned(), "run-3".to_owned()],
            "the run in the middle wrote no history, so it takes no round number"
        );
        assert!(
            store
                .turn_runs_for_session("no-such-session")
                .expect("turns")
                .is_empty()
        );
    }

    /// `sessions.updated_at` and `last_activity_at` are written by the same
    /// `UPDATE` on every turn — and **nothing in the public API reads
    /// `updated_at`**, so the witness has to be the column itself, read
    /// straight out of SQLite. Asserting it against its sibling field would
    /// only prove the two are spelled the same way twice.
    #[test]
    fn a_turn_advances_both_of_the_session_row_s_timestamps() {
        let (_dir, mut store) = temp_store();
        store
            .create_session(&NewSession {
                session_id: "sess-1".to_owned(),
                title: "why is the sky blue?".to_owned(),
                title_source: "derived".to_owned(),
                first_user_message: Some("why is the sky blue?".to_owned()),
                project_id: None,
                created_at: 100,
            })
            .expect("session");
        let both = |store: &Store| -> (i64, i64) {
            store
                .connection
                .query_row(
                    "SELECT updated_at, last_activity_at FROM sessions
                      WHERE session_id = ?1",
                    params!["sess-1"],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read both timestamps")
        };
        assert_eq!(
            both(&store),
            (100, 100),
            "a new conversation starts level with its creation instant"
        );

        accepted(&mut store, &run_at("run-1", Some("sess-1"), 100));
        append_turns(&mut store, "run-1", &[1], 160);

        assert_eq!(
            both(&store),
            (160, 160),
            "the turn moved both — `answer_of`'s 160 beats the row's 100, and \
             dropping either half of the `SET` leaves the other behind"
        );
    }

    /// The clamp on the column "recency 排序" actually orders conversations
    /// by: a turn whose clock has stepped back must not pull
    /// `last_activity_at` behind where the row already stopped. Its two
    /// sibling clamps (`runs.created_at`, and the history axis against the
    /// round axis) have named witnesses; this one did not — so `.max` flipped
    /// to `.min`, or the clamp dropped in favour of `answer.created_at`,
    /// would reorder the session list and nothing would say so.
    #[test]
    fn a_clock_step_cannot_pull_last_activity_behind_where_the_row_stopped() {
        let (_dir, mut store) = temp_store();
        store
            .create_session(&NewSession {
                session_id: "sess-1".to_owned(),
                title: "why is the sky blue?".to_owned(),
                title_source: "derived".to_owned(),
                first_user_message: Some("why is the sky blue?".to_owned()),
                project_id: None,
                created_at: 100,
            })
            .expect("session");

        accepted(&mut store, &run_at("run-1", Some("sess-1"), 100));
        append_turns(&mut store, "run-1", &[1], 160);

        // …then a turn written after the clock stepped back to 120.
        accepted(&mut store, &run_at("run-2", Some("sess-1"), 120));
        let mut late = answer_of("run-2");
        late.created_at = 120;
        let mut row = turn_row("hist-run-2-1", "run-2", 1, 120);
        row.session_id = Some("sess-1".to_owned());
        store
            .commit_run_answer(
                "run-2",
                &late,
                &[row],
                &event(momotaro_contracts::run_event::ANSWER_FINALIZED, 120),
            )
            .expect("answer");

        let last: i64 = store
            .connection
            .query_row(
                "SELECT last_activity_at FROM sessions WHERE session_id = ?1",
                params!["sess-1"],
                |row| row.get(0),
            )
            .expect("read last_activity_at");
        assert_eq!(
            last, 160,
            "the clock step left recency where the row already stopped; a clamp \
             that goes the other way would report 120 and put this conversation \
             behind one written earlier"
        );
    }

    /// `ensure_run` is the only thing between **all six** write paths and a
    /// row for a run that does not exist — and the DDL declares no
    /// `REFERENCES`, so SQLite would write the orphan happily. Each path is
    /// driven once: deleting `ensure_run` at that call site means no
    /// `RunMissing`, so the match **is** the witness. There is deliberately no row
    /// count: every `Err` rolls its transaction back, so a count after a
    /// guaranteed rollback asserts nothing on any reachable path.
    #[test]
    fn every_write_path_refuses_a_run_that_does_not_exist() {
        let (_dir, mut store) = temp_store();
        let ctx = event(momotaro_contracts::run_event::CONTEXT_BUILT, 100);
        let refused = |label: &str, result: Result<(), StoreError>| {
            assert!(
                matches!(result, Err(StoreError::RunMissing { .. })),
                "{label} must refuse a missing run rather than write an orphan: \
                 {result:?}"
            );
        };

        refused("append_run_event", store.append_run_event("ghost", &ctx));
        refused(
            "commit_run_context",
            store.commit_run_context("ghost", &[], &ctx),
        );
        refused(
            "commit_run_verifications",
            store.commit_run_verifications("ghost", &[], &ctx),
        );
        refused(
            "commit_run_answer",
            store.commit_run_answer("ghost", &answer_of("ghost"), &[], &ctx),
        );
        refused(
            "commit_run_retrieval",
            store.commit_run_retrieval(
                "ghost",
                &RetrievalRecord {
                    retrieval_id: "ret-ghost".to_owned(),
                    run_id: "ghost".to_owned(),
                    query: "q".to_owned(),
                    normalized_query: "q".to_owned(),
                    strategy_json: "{}".to_owned(),
                    created_at: 100,
                    hits: Vec::new(),
                },
                &ctx,
            ),
        );
        refused(
            "commit_run_llm_turn",
            store.commit_run_llm_turn(
                "ghost",
                &LlmTurnRecord {
                    turn_id: "turn-ghost".to_owned(),
                    run_id: "ghost".to_owned(),
                    seq: 1,
                    provider: "fake".to_owned(),
                    model: "fake-model".to_owned(),
                    request_hash: "rh".to_owned(),
                    status: "finished".to_owned(),
                    input_tokens: None,
                    output_tokens: None,
                    cached_tokens: None,
                    billable_tokens: None,
                    slot: None,
                    reasoning_tokens: None,
                    raw_ref: None,
                    created_at: 100,
                    finished_at: None,
                },
                &ctx,
            ),
        );
    }

    /// The terminal event, not the status column, decides whether the sweep
    /// takes a run — three states, and the middle one is the predicate
    /// `sweep_interrupted_runs` uses: a run that still carries a terminal
    /// event is closed whatever its `status` reads.
    #[test]
    fn a_terminal_event_stops_the_sweep_whatever_the_status_column_says() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-live", Some("sess-1"), 100));
        accepted(&mut store, &run_at("run-done", Some("sess-1"), 101));

        // Contradictory on purpose: status still says running while a
        // terminal event is present. The event wins (`harness.md` §1: status
        // is a projection). `commit_run_terminal` would move the status too,
        // so this shape is built by hand with raw SQL — the API cannot make
        // it, which is exactly why it is worth pinning.
        store
            .connection
            .execute(
                "UPDATE runs SET status = 'running' WHERE run_id = 'run-done'",
                [],
            )
            .expect("flip status");
        store
            .connection
            .execute(
                "INSERT INTO run_events (run_id, seq, type, data_json, created_at)
                 VALUES ('run-done', 99, 'run.finished', '{}', 200)",
                [],
            )
            .expect("terminal event by hand");

        let mut swept = store.sweep_interrupted_runs().expect("sweep");
        swept.sort();
        assert_eq!(
            swept,
            ["run-live".to_owned()],
            "the open run is reaped; a terminal event closes the other one \
             whatever its status column says — `ACTIVE_RUN_PREDICATE` reads the \
             event, not the column"
        );
    }

    #[test]
    fn a_session_is_created_once_and_a_duplicate_is_refused() {
        let (_dir, mut store) = temp_store();
        assert!(!store.session_exists("sess-new").expect("exists"));

        store
            .create_session(&NewSession {
                session_id: "sess-new".to_owned(),
                title: "why is the sky blue?".to_owned(),
                title_source: "derived".to_owned(),
                first_user_message: Some("why is the sky blue?".to_owned()),
                project_id: None,
                created_at: 100,
            })
            .expect("create session");
        assert!(store.session_exists("sess-new").expect("exists"));
        // Read back through the list the DDL's own comments promise
        // (`NewSession`: title/first_user_message are 列表预览). Zeroing any
        // of the four on the way in has to move this.
        assert_eq!(
            store.sessions().expect("sessions"),
            [SessionRecord {
                session_id: "sess-new".to_owned(),
                title: "why is the sky blue?".to_owned(),
                title_source: "derived".to_owned(),
                first_user_message: Some("why is the sky blue?".to_owned()),
                created_at: 100,
                last_activity_at: Some(100),
            }],
            "every column `create_session` writes comes back as written — \
             `last_activity_at` starts level with `created_at` and only a turn \
             moves it (`a_continued_session_advances_last_activity…`)"
        );

        let duplicate = store.create_session(&NewSession {
            session_id: "sess-new".to_owned(),
            title: "second".to_owned(),
            title_source: "derived".to_owned(),
            first_user_message: None,
            project_id: None,
            created_at: 200,
        });
        assert!(
            duplicate.is_err(),
            "the primary key refuses a second session rather than silently adding one"
        );
        assert!(!store.session_exists("sess-other").expect("exists"));
    }

    /// The two readers key off two different columns — `turn_runs_for_session`
    /// on `runs.session_id`, `history_for_session` on
    /// `history_items.session_id` — so a row written with the wrong one splits
    /// them: the round axis counts a turn the replay cannot see. That is a
    /// phantom round, and the guard band then protects one turn fewer than
    /// the session actually has, which is the opposite of fail-closed.
    #[test]
    fn a_history_row_that_disagrees_with_its_run_is_refused() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-1", Some("sess-1"), 100));

        let mut row = turn_row("hist-1", "run-1", 1, 100);
        row.session_id = Some("sess-2".to_owned());
        let error = store
            .commit_run_answer(
                "run-1",
                &answer_of("run-1"),
                &[row],
                &event(momotaro_contracts::run_event::ANSWER_FINALIZED, 100),
            )
            .expect_err("the row names a session its run does not belong to");
        assert!(
            error.to_string().contains("session_id"),
            "the refusal names the column: {error}"
        );
        assert_eq!(
            store.history_for_session("sess-1").expect("history").len(),
            0,
            "and nothing landed — the unit is all or nothing"
        );
        assert_eq!(
            store.run_count().expect("count"),
            1,
            "the run itself is untouched"
        );
    }

    /// `context_items_for_session` is scoped to the session's **turn** runs —
    /// the same predicate `turn_runs_for_session` numbers rounds by. A reader
    /// that returned everything, or nothing, would hand the ledger and the
    /// guard band two different ideas of what a turn is.
    #[test]
    fn the_context_items_reader_is_scoped_to_the_sessions_turn_runs() {
        let (_dir, mut store) = temp_store();

        // A turn run in sess-1: it wrote history, so its items count.
        accepted(&mut store, &run_at("run-turn", Some("sess-1"), 100));
        append_turns(&mut store, "run-turn", &[1], 100);
        store
            .commit_run_context(
                "run-turn",
                &[context_item("run-turn", 1)],
                &event(momotaro_contracts::run_event::CONTEXT_BUILT, 100),
            )
            .expect("context");

        // Same session, never wrote history: not a turn run, no items.
        accepted(&mut store, &run_at("run-silent", Some("sess-1"), 300));
        store
            .commit_run_context(
                "run-silent",
                &[context_item("run-silent", 1)],
                &event(momotaro_contracts::run_event::CONTEXT_BUILT, 300),
            )
            .expect("context");

        // Another session's turn run must not leak across.
        accepted(&mut store, &run_at("run-other", Some("sess-2"), 400));
        append_turns(&mut store, "run-other", &[1], 400);
        store
            .commit_run_context(
                "run-other",
                &[context_item("run-other", 1)],
                &event(momotaro_contracts::run_event::CONTEXT_BUILT, 400),
            )
            .expect("context");

        let items = store.context_items_for_session("sess-1").expect("items");
        assert_eq!(
            items.len(),
            1,
            "only the turn run's items come back — `run-silent` wrote none"
        );
        assert_eq!(items[0].run_id, "run-turn");
        assert_eq!(
            store
                .context_items_for_session("sess-2")
                .expect("other session")
                .len(),
            1,
            "the other session keeps its own turn run"
        );
        assert!(
            store
                .context_items_for_session("ghost")
                .expect("unknown session")
                .is_empty(),
            "an unknown session has no context items"
        );
    }

    /// A clock step must not pull the replay axis away from the round axis
    /// (`context-retrieval.md` §3). The round axis clamps
    /// `runs.created_at`; if `history_items.created_at` did not, the
    /// conversation would replay in the **opposite** order to the rounds
    /// that number it, and `guard_band_from_round` would walk the history
    /// backwards — silently, under exactly the condition the clamp exists
    /// for.
    #[test]
    fn a_clock_step_cannot_invert_the_replay_axis_against_the_round_axis() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-1", Some("sess-1"), 1_000));
        append_turns(&mut store, "run-1", &[1], 1_000);
        // …then the clock steps backwards.
        accepted(&mut store, &run_at("run-2", Some("sess-1"), 500));
        append_turns(&mut store, "run-2", &[1], 500);

        let rounds = store.turn_runs_for_session("sess-1").expect("rounds");
        let rows = store.history_for_session("sess-1").expect("history");
        let replay: Vec<&str> = rows.iter().map(|row| row.run_id.as_str()).collect();
        assert_eq!(
            rounds,
            vec!["run-1".to_owned(), "run-2".to_owned()],
            "the clamped round axis keeps insertion order across the step"
        );
        assert_eq!(
            replay,
            vec!["run-1", "run-2"],
            "replay order must be the same sequence as the rounds, or the \
             guard band walks the history backwards: rounds={rounds:?}"
        );
    }

    /// Two turn runs in the *same second*: `created_at` alone cannot order
    /// them — SQLite leaves a full tie in an undefined sequence — so
    /// `run_id` is the second key `context-retrieval.md` §3 calls
    /// 「缺一不可」. The fixture inserts them **in reverse id order**, so only
    /// that tiebreak can put them right; dropping it from either query hands
    /// the guard band the wrong two rounds while the replay says otherwise.
    #[test]
    fn two_turn_runs_in_the_same_second_are_ordered_by_run_id() {
        let (_dir, mut store) = temp_store();
        accepted(&mut store, &run_at("run-zzz", Some("sess-1"), 100));
        append_turns(&mut store, "run-zzz", &[1], 100);
        accepted(&mut store, &run_at("run-aaa", Some("sess-1"), 100));
        append_turns(&mut store, "run-aaa", &[1], 100);

        assert_eq!(
            store.turn_runs_for_session("sess-1").expect("rounds"),
            vec!["run-aaa".to_owned(), "run-zzz".to_owned()],
            "same created_at ⇒ run_id decides; insertion order is not the contract"
        );
        let rows = store.history_for_session("sess-1").expect("history");
        let replay: Vec<&str> = rows.iter().map(|row| row.run_id.as_str()).collect();
        assert_eq!(
            replay,
            vec!["run-aaa", "run-zzz"],
            "the replay axis has to land on the same tiebreak as the rounds"
        );
    }
}
