//! Sequential forward-only schema migrations.
//!
//! Every step is exactly one transaction and the version stamp
//! (`PRAGMA user_version`) is the last statement inside it: a crash
//! mid-migration rolls back to the previous consistent version instead of
//! leaving a half-migrated database.

use rusqlite::Connection;

use crate::StoreError;

/// Ordered forward-only migration steps. Applying every entry with a version
/// greater than the stored version brings the database to the newest schema.
const MIGRATIONS: &[(u32, &str)] = &[
    // 0 -> 1 baseline: `schema_meta` is created by `init_schema` before the
    // runner starts, so this step only records the version.
    (1, ""),
    // 1 -> 2: canonical source revisions and chunks.
    (
        2,
        "CREATE TABLE IF NOT EXISTS source_revisions (
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
CREATE TABLE IF NOT EXISTS chunks (
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
CREATE INDEX IF NOT EXISTS idx_chunks_rev ON chunks(source_key, revision_hash);
CREATE INDEX IF NOT EXISTS idx_revisions_current ON source_revisions(source_key) WHERE is_current = 1;",
    ),
    // 2 -> 3: key scheme v2 — the raw spelling of a name beside its key.
    (3, "ALTER TABLE source_revisions ADD COLUMN raw_name TEXT;"),
    // 3 -> 4: the store's own audit log. `id` is the rowid: no
    // `AUTOINCREMENT`, which SQLite's own advice prices at extra CPU, memory,
    // disk and IO for a strictly worse guarantee than insertion order.
    // `UNIQUE (batch_id, seq)` carries an autoindex that already serves
    // `WHERE batch_id = ?` through its leftmost prefix, so no second index on
    // those columns; the two partial indexes cover the lookup columns that
    // would otherwise scan the domain or the source history.
    (
        4,
        "CREATE TABLE IF NOT EXISTS store_events (
  id INTEGER PRIMARY KEY,
  occurred_at INTEGER NOT NULL,
  recorded_at INTEGER NOT NULL,
  batch_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  domain TEXT NOT NULL,
  type TEXT NOT NULL,
  source_key TEXT,
  outcome TEXT,
  reason_code TEXT,
  message TEXT,
  data_json TEXT NOT NULL,
  external_id TEXT,
  UNIQUE (batch_id, seq)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_store_events_external
  ON store_events(domain, external_id) WHERE external_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_store_events_source
  ON store_events(source_key) WHERE source_key IS NOT NULL;",
    ),
    // 4 -> 5: the canonical tables, batched into one step when three work items
    // merged. The eleven run-time tables are declared in full
    // (the `decision_rationale` / `assumptions` / `supersedes`
    // columns on `answers` and `run_context_items` are the
    // early-embed columns, appended at the end in the schema-3 `raw_name` manner),
    // and `graph_links` is the eight-column derived table in the table's own
    // column order — `to_source_key` is NULL exactly
    // for a dangling link, which stays a first-class row, and `raw_target`
    // keeps the target as written for a later resolve. Row identity and the
    // replace granularity of `graph_links` were decided with the edge-building
    // work: identity is the as-written six-tuple, replacement is per
    // `from_source_key`, and the table is derived — it can be rebuilt with
    // a different shape without touching canonical data. The tables already
    // present (source_revisions, chunks, store_events) are not rebuilt.
    //
    // The PRIMARY KEY / UNIQUE declarations carry their own autoindexes, and
    // those serve the keyed lookups these tables were designed around. The four
    // `run_id` lookups they do NOT serve (retrievals, llm_turns, verifications,
    // answers) got their indexes in step 7 — `run_trace` scans those tables per
    // run. The one explicit index declared here is `graph_links(from_source_key)`
    // — that table declares no key, and every projection replace deletes by
    // `from_source_key`.
    (
        5,
        "CREATE TABLE IF NOT EXISTS sessions (
  session_id TEXT PRIMARY KEY,
  parent_session_id TEXT,
  project_id TEXT,                   -- 可空；创建时绑定，此后不可变
  title TEXT NOT NULL,
  title_source TEXT NOT NULL DEFAULT 'derived',  -- derived | user | llm；user 改名后永不被自动标题覆盖；本构建只写 derived
  profile_id TEXT,                   -- 本构建不写；无 profiles 表
  first_user_message TEXT,          -- 列表预览与检索喂料
  archived INTEGER NOT NULL DEFAULT 0,
  archived_at INTEGER,
  pinned INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  last_activity_at INTEGER          -- recency 排序，区别于 created_at
);
CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY,
  session_id TEXT,
  parent_run_id TEXT,
  project_id TEXT,
  graph_id TEXT,
  node_id TEXT,
  attempt INTEGER NOT NULL DEFAULT 1,
  mode TEXT NOT NULL,              -- ask | search | paper_add | spark（封闭集）
  query TEXT NOT NULL,
  status TEXT NOT NULL,            -- accepted | running | finished | failed | cancelled
  client_request_id TEXT UNIQUE,
  run_spec_hash TEXT NOT NULL,
  run_spec_json TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  context_window INTEGER,
  max_output_tokens INTEGER,
  result_digest TEXT,               -- 图节点蒸馏结果（1-2K token 上限）
  failure_note TEXT,                -- 失败 run 的部分产出+标注，不静默吞（图节点共识）
  deadline_at INTEGER,              -- 显式超时（图节点）
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE TABLE IF NOT EXISTS run_events (
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  type TEXT NOT NULL,
  data_json TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (run_id, seq)
);
CREATE TABLE IF NOT EXISTS history_items (
  history_id TEXT PRIMARY KEY,
  session_id TEXT,
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  role TEXT NOT NULL,
  content_kind TEXT NOT NULL,
  body TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  source_ref_json TEXT,
  token_estimate INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  occurred_at INTEGER,             -- 事件真实发生时间（重放/导入时≠created_at）
  UNIQUE (run_id, seq)
);
CREATE TABLE IF NOT EXISTS retrievals (
  retrieval_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  query TEXT NOT NULL,
  normalized_query TEXT NOT NULL,
  strategy_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS retrieval_hits (
  retrieval_id TEXT NOT NULL,
  rank INTEGER NOT NULL,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  chunk_id TEXT NOT NULL,
  chunk_hash TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  score REAL,
  included INTEGER NOT NULL,
  omitted_reason TEXT,
  PRIMARY KEY (retrieval_id, chunk_id)
);
CREATE TABLE IF NOT EXISTS run_context_items (
  run_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  fragment_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  role TEXT NOT NULL,
  content_kind TEXT NOT NULL,
  body TEXT NOT NULL,
  source_ref_json TEXT,
  token_estimate INTEGER NOT NULL,
  body_hash TEXT NOT NULL,
  decision_rationale TEXT,          -- 埋点（≤50 token 决策摘要）
  assumptions TEXT,                 -- 埋点
  supersedes TEXT,                  -- 埋点：指向被取代工件
  PRIMARY KEY (run_id, ordinal)
);
CREATE TABLE IF NOT EXISTS llm_turns (
  turn_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  status TEXT NOT NULL,            -- started | finished | failed
  input_tokens INTEGER,
  output_tokens INTEGER,
  cached_tokens INTEGER,           -- provider 缓存命中的输入 token
  billable_tokens INTEGER,         -- 实际计费口径（折扣后），未知为 NULL
  slot TEXT,                       -- answer | utility（RunSpec 冻结快照）
  reasoning_tokens INTEGER,        -- 思考 token（按输出价计费口径）
  raw_ref TEXT,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE TABLE IF NOT EXISTS answers (
  answer_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  text TEXT NOT NULL,
  prompt_hash TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  decision_rationale TEXT,          -- 埋点（≤50 token 决策摘要）
  assumptions TEXT,                 -- 埋点
  supersedes TEXT                   -- 埋点：指向被取代工件
);
CREATE TABLE IF NOT EXISTS answer_citations (
  answer_id TEXT NOT NULL,
  citation_index INTEGER NOT NULL,
  source_key TEXT NOT NULL,
  revision_hash TEXT NOT NULL,
  chunk_id TEXT NOT NULL,
  chunk_hash TEXT NOT NULL,
  locator_json TEXT NOT NULL,
  quoted_text TEXT NOT NULL,
  verified INTEGER NOT NULL,
  PRIMARY KEY (answer_id, citation_index)
);
CREATE TABLE IF NOT EXISTS verifications (
  verification_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  answer_id TEXT,
  kind TEXT NOT NULL,              -- citation | source_integrity | math
  status TEXT NOT NULL,            -- passed | failed | unsupported | skipped
  details_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS graph_links (
  from_source_key TEXT NOT NULL,   -- the note the link is written in
  to_source_key TEXT,              -- NULL for a dangling link — a first-class row, not a discard
  raw_target TEXT NOT NULL,        -- the target exactly as written, kept so a later resolve can backfill
  edge_kind TEXT NOT NULL,         -- how the edge was expressed
  link_source TEXT NOT NULL,       -- which syntax produced it
  anchor_kind TEXT,                -- what the anchor points at, when the link has one
  anchor_raw TEXT,                 -- the anchor as written
  created_at INTEGER NOT NULL      -- the ingested_at of the revision carrying the link
);
CREATE INDEX IF NOT EXISTS idx_graph_links_from ON graph_links (from_source_key);",
    ),
    // 5 -> 6: `idx_graph_links_from` joined the step-5 SQL after that step had
    // already shipped, so databases stamped schema 5 by builds in the window
    // between the two carry the same version number without the index — and a
    // migration chain only runs steps *greater* than the stored version. This
    // step converges both shapes; it is a no-op where the index exists. Shipped
    // steps are never edited again (step 5 itself exists in two historical
    // spellings, which this step is what makes equivalent).
    (
        6,
        "CREATE INDEX IF NOT EXISTS idx_graph_links_from ON graph_links (from_source_key);",
    ),
    // 6 -> 7: an index on the child key of each `run_id` relationship.
    //
    // SQLite's foreign-key guidance ("Required and Suggested Database
    // Indexes") is the standing guidance: "Indices are not required for child
    // key columns but they are almost always beneficial." … "So, in most real
    // systems, an index should be created on the child key columns of each
    // foreign key constraint."
    //
    // Read against this schema, precisely: the DDL declares no `REFERENCES`
    // clause, so SQLite does not enforce these as foreign keys — they are
    // child keys by construction, and `run_trace` is what reads them
    // (`WHERE run_id = ?1` on all four). Without an index such queries
    // "are forced to do a linear scan of the entire child table". The other
    // `run_id` tables already have a covering index of their own: `runs`
    // (primary key), `run_events` / `run_context_items` / `history_items`
    // (a key whose leftmost column is `run_id`).
    //
    // Each index is dropped before it is created: `CREATE INDEX IF NOT EXISTS`
    // checks the *name* across the whole schema, not the table, so a store
    // where `idx_answers_run` already existed on some other table would skip
    // the creation, stamp schema 7, and leave `answers` uncovered while `doctor`
    // reported ready. A name this build owns in a database this build owns is
    // ours to replace; an index is derived, and rebuilding it inside the step's
    // transaction costs nothing on the ordinary path where it does not exist.
    (
        7,
        "DROP INDEX IF EXISTS idx_retrievals_run;
CREATE INDEX idx_retrievals_run ON retrievals(run_id);
DROP INDEX IF EXISTS idx_llm_turns_run;
CREATE INDEX idx_llm_turns_run ON llm_turns(run_id);
DROP INDEX IF EXISTS idx_answers_run;
CREATE INDEX idx_answers_run ON answers(run_id);
DROP INDEX IF EXISTS idx_verifications_run;
CREATE INDEX idx_verifications_run ON verifications(run_id);",
    ),
    // 7 -> 8: an index on `session_id` for the two tables a continued `ask`
    // reads in full on every turn.
    //
    // SQLite's own rule: "The general rule is that indexes are only useful if
    // there are WHERE-clause constraints on the left-most columns of the
    // index." `history_for_session` constrains `session_id` and nothing else;
    // `turn_runs_for_session` constrains it first and adds `EXISTS(...)`
    // after, so the left-most-column rule is satisfied by both. Without one,
    // `ask --session` reads every `body` blob in a session's history through
    // a full scan, and `runs` is scanned the same way to number the rounds;
    // `history_items` grows by two rows per ask forever, so this is not a
    // small-table case.
    //
    // Same DROP-then-CREATE discipline as step 7 (see there): `IF NOT EXISTS`
    // matches the *name* across the whole schema, not the table, so a stale
    // name elsewhere would skip creation, stamp schema 8, and leave the column
    // uncovered while `doctor` reported ready.
    (
        8,
        "DROP INDEX IF EXISTS idx_runs_session;
CREATE INDEX idx_runs_session ON runs(session_id);
DROP INDEX IF EXISTS idx_history_items_session;
CREATE INDEX idx_history_items_session ON history_items(session_id);",
    ),
    // 8 -> 9: turn ordering integrity for `run_trace`.
    //
    // `run_trace` replays a run's turns by `ORDER BY seq` (runs.rs), and
    // `commit_run_llm_turn` stores the caller-declared `seq` verbatim, so two
    // turns of one run carrying the same number would leave the replay order
    // to SQLite's unspecified scan order. The unique key mirrors the defense
    // the sibling projection table already has — `run_context_items`'
    // `PRIMARY KEY (run_id, ordinal)` — while `llm_turns` keeps `turn_id` as
    // its primary key and the caller's handle. It replaces
    // `idx_llm_turns_run`: the new key's leftmost column serves every
    // `WHERE run_id = ?` lookup the dropped index did. Same DROP-then-CREATE
    // discipline as steps 7 and 8 (see step 7): `IF NOT EXISTS` matches the
    // *name*, so a stale index of this name on another table would skip
    // creation, stamp schema 9, and leave the pair unprotected while `doctor`
    // reported ready.
    (
        9,
        "DROP INDEX IF EXISTS idx_llm_turns_run;
DROP INDEX IF EXISTS idx_llm_turns_run_seq;
CREATE UNIQUE INDEX idx_llm_turns_run_seq ON llm_turns(run_id, seq);",
    ),
];

/// Applies every migration newer than `version`.
pub(crate) fn apply_pending(connection: &mut Connection, version: u32) -> Result<(), StoreError> {
    for &(target, sql) in MIGRATIONS {
        if target > version {
            apply_step(connection, target, sql)?;
        }
    }

    Ok(())
}

/// Applies one step: all of its SQL and the version stamp in a single
/// transaction, with the stamp written last. A failure rolls the whole
/// step back and leaves the previous version in place.
fn apply_step(connection: &mut Connection, target: u32, sql: &str) -> Result<(), StoreError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(sql)?;
    // The version stamp is the last statement in the transaction.
    transaction.execute_batch(&format!("PRAGMA user_version = {target};"))?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    /// `apply_pending` must skip the step the store already carries.
    ///
    /// The guard is `target > version`, and turning it into `>=` re-runs one
    /// step. That is harmless for `CREATE … IF NOT EXISTS`, but step 3 is
    /// `ALTER TABLE source_revisions ADD COLUMN raw_name` — not idempotent —
    /// so a store reporting schema 3 with the column already present would refuse to
    /// open at all. No fixture could tell either way: the intermediate-version
    /// ones (`seed_store_at_schema_2`, `seed_store_at_schema_4`, the schema-6 and
    /// schema-7 rewinds) re-run steps that are all `CREATE … IF NOT EXISTS` shaped,
    /// so re-running them is a no-op. Step 3 is the only migration that notices — the
    /// rest of the table is idempotent under a repeat.
    /// table is idempotent under a repeat.
    #[test]
    fn a_store_already_at_a_step_is_not_asked_to_take_it_again() {
        let mut connection = Connection::open_in_memory().expect("open");
        // The schema exactly as step 3 left it: steps 1–3, nothing later, so
        // that step 3 is the one a `>=` would try to repeat.
        let mut applied = 0;
        for &(target, sql) in MIGRATIONS {
            if target > 3 {
                break;
            }
            connection
                .execute_batch(sql)
                .expect("apply the early steps");
            applied = target;
        }
        assert_eq!(applied, 3, "the fixture really stops at step 3");
        connection
            .execute_batch("PRAGMA user_version = 3;")
            .expect("stamp the version");

        apply_pending(&mut connection, 3).expect("steps 4..8 apply cleanly");

        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read the version back");
        let newest = MIGRATIONS.last().expect("the table is not empty").0;
        assert_eq!(version, newest, "…and the store reaches the newest step");
    }

    #[test]
    fn every_step_is_ordered_and_unique() {
        let mut seen = Vec::new();
        for &(target, _) in MIGRATIONS {
            assert!(
                seen.last().is_none_or(|previous| previous < &target),
                "migration targets must strictly increase: {target}"
            );
            seen.push(target);
        }
        assert_eq!(seen.first(), Some(&1), "the chain starts at 1");
    }

    #[test]
    fn a_failing_step_rolls_back_and_keeps_the_previous_version() {
        let mut connection = Connection::open_in_memory().expect("open in-memory store");
        apply_step(&mut connection, 1, "CREATE TABLE one (x);").expect("first step succeeds");

        // The step creates a table and then fails: the table must vanish and
        // the version must stay where the last successful step left it.
        apply_step(
            &mut connection,
            2,
            "CREATE TABLE two (x); CREATE TABLE two (x);",
        )
        .expect_err("the second statement cannot succeed");

        assert_eq!(
            version(&connection),
            1,
            "the stamp stays at the last good step"
        );
        assert!(
            table_exists(&connection, "one"),
            "the earlier step survives"
        );
        assert!(
            !table_exists(&connection, "two"),
            "the failed step's DDL is rolled back"
        );
    }

    #[test]
    fn the_stamp_wins_over_any_version_the_step_sql_writes() {
        let mut connection = Connection::open_in_memory().expect("open in-memory store");

        // A step whose own SQL tries to write a different version: only a
        // stamp that runs LAST can leave the step's declared version behind.
        // Moving `PRAGMA user_version = {target}` before the SQL (or deleting
        // it) makes this assertion red — the ordering is the crash-safety
        // contract, not a comment. The containment half (the stamp runs
        // inside the step's transaction) is pinned by
        // `the_stamp_is_the_last_statement_inside_the_step_transaction`.
        apply_step(
            &mut connection,
            7,
            "CREATE TABLE t (x); PRAGMA user_version = 99;",
        )
        .expect("the step applies");

        assert_eq!(
            version(&connection),
            7,
            "the declared version wins: the stamp is written after the step's SQL"
        );
    }

    #[test]
    fn the_stamp_is_the_last_statement_inside_the_step_transaction() {
        // Behaviour cannot observe the crash window between two separate
        // transactions, so the containment property is pinned structurally:
        // within `apply_step` every write goes through the transaction handle,
        // and the version stamp sits between the step's SQL and the commit —
        // never after it, never on the raw connection. The needle literals
        // live here in the tests module, so the slice below cannot count them.
        let source = include_str!("migration.rs");
        let body = source
            .split("fn apply_step")
            .nth(1)
            .expect("apply_step exists")
            .split("\n}")
            .next()
            .expect("apply_step body");

        assert_eq!(
            body.matches("connection.").count(),
            1,
            "apply_step reaches the database only through the transaction: {body}"
        );
        assert!(
            body.contains("connection.transaction()"),
            "and that one reach is the transaction itself: {body}"
        );
        let sql_at = body
            .find("execute_batch(sql)")
            .expect("the step's SQL runs");
        let stamp_at = body.find("PRAGMA user_version").expect("the stamp runs");
        let commit_at = body
            .find("transaction.commit()")
            .expect("the transaction commits");
        assert!(
            sql_at < stamp_at,
            "the stamp follows the step's SQL: {body}"
        );
        assert!(
            stamp_at < commit_at,
            "the stamp precedes the commit — inside the same transaction: {body}"
        );
    }

    #[test]
    fn schema_9_makes_the_turn_order_key_unique() {
        let mut connection = Connection::open_in_memory().expect("open in-memory store");
        apply_pending(&mut connection, 0).expect("every step applies");

        let unique_key = connection
            .query_row(
                "SELECT 1 FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_llm_turns_run_seq'",
                [],
                |_| Ok(()),
            )
            .optional()
            .expect("query sqlite_master");
        assert!(
            unique_key.is_some(),
            "the unique turn-order key exists once migrations settle"
        );
        assert!(
            connection
                .query_row(
                    "SELECT 1 FROM sqlite_master
                     WHERE type = 'index' AND name = 'idx_llm_turns_run'",
                    [],
                    |_| Ok(()),
                )
                .optional()
                .expect("query sqlite_master")
                .is_none(),
            "the superseded single-column index is gone"
        );

        let insert = |turn_id: &str| {
            connection.execute(
                "INSERT INTO llm_turns (turn_id, run_id, seq, provider, model, request_hash, status, created_at)
                 VALUES (?1, 'run-1', 1, 'p', 'm', 'h', 'finished', 1)",
                [turn_id],
            )
        };
        insert("turn-1").expect("the first turn of the run");
        assert!(
            insert("turn-2").is_err(),
            "a second turn with the same (run_id, seq) is refused"
        );
    }
    fn version(connection: &Connection) -> u32 {
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version")
    }

    fn table_exists(connection: &Connection, name: &str) -> bool {
        connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |_| Ok(()),
            )
            .optional()
            .expect("query sqlite_master")
            .is_some()
    }
}
