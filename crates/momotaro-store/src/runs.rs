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
                    "SELECT run_id FROM runs WHERE client_request_id = ?1",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(run_id) = existing {
                tx.rollback()?;
                return Ok(CreateRunOutcome::Existing { run_id });
            }
        }
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
                run.created_at,
            ],
        )?;
        insert_event_tx(&tx, &run.run_id, event)?;
        tx.commit()?;
        Ok(CreateRunOutcome::Created {
            run_id: run.run_id.clone(),
        })
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
        for item in history {
            if item.run_id != run_id {
                return Err(invalid_value("run_id", item.run_id.clone()));
            }
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
                    item.created_at,
                    item.occurred_at,
                ],
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

    /// The deterministic crash sweep (`harness.md` §2): every run left
    /// `accepted` or `running` with no terminal event is closed as
    /// `failed(interrupted)` — the terminal unit per run, so an interrupted
    /// sweep is itself resumable. Returns the swept run ids.
    ///
    /// Callers: the run engine's store opens (`ask` / `trace`). Read-only
    /// inspectors do not sweep — `doctor` writes nothing (`usage.md`), so
    /// "the run engine's open sweeps" is where the rule is implemented.
    ///
    /// Harness §2's third clause (「属于已死进程的行」) is implied by 0.x's
    /// single-writer discipline (harness §5: no cross-process writers), not
    /// evaluated here — a live run cannot coexist with this sweep in one
    /// process. A future desktop that opens a second writer mid-run owes the
    /// clause a real liveness check before that happens.
    pub fn sweep_interrupted_runs(&mut self) -> Result<Vec<String>, StoreError> {
        let pending: Vec<String> = self
            .connection
            .prepare(
                "SELECT run_id FROM runs
                 WHERE status IN ('accepted', 'running')
                   AND NOT EXISTS (
                     SELECT 1 FROM run_events
                     WHERE run_events.run_id = runs.run_id
                       AND type IN ('run.finished', 'run.failed', 'run.cancelled')
                   )",
            )?
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
    use momotaro_contracts::{ModelSlot, RunMode};

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mut store = Store::open(dir.path().join("momotaro.db")).expect("open store");
        store.init_schema().expect("init schema");
        (dir, store)
    }

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
}
