//! Service functions used by Momotaro surfaces.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod index_service;
mod reconcile;
mod watch;

pub mod ask;

pub use ask::{
    AskOptions, AskReport, CitationVerdict, IntegrityVerdict, ProviderTransport, ask_workspace,
    format_ask, format_trace, trace_workspace, validate_ask_policy,
};
pub use index_service::{
    RebuildReport, SearchHit, SearchOutput, coverage_report, index_workspace, index_workspace_with,
    rebuild_index, search_workspace,
};
pub use reconcile::{ReconcileReport, format_reconcile, reconcile};
pub use watch::{WatchLine, format_watch_line, watch};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use momotaro_contracts::{
    CoverageReport, HealthReport, IndexFileError, IndexFileErrorKind, IndexPolicy, IndexReport,
    SchemaVersion, SqliteFacts, StatsReport, VaultScope, WorkspaceStatus,
};
use momotaro_ingest::IngestError;
use momotaro_retrieve::RetrieveError;
use momotaro_store::{Store, StoreError};
use serde::Deserialize;
use thiserror::Error;

use crate::index_service::index_unusable_message;

const CONFIG_FILE: &str = "momotaro.toml";
const DATA_DIR: &str = ".momotaro";
const DATABASE_FILE: &str = "momotaro.db";

/// Service-level failure.
#[derive(Debug, Error)]
pub enum RunError {
    /// The workspace configuration file could not be read.
    #[error("could not read {path}: {source}")]
    ConfigRead {
        /// Configuration path.
        path: PathBuf,
        /// Underlying IO error.
        source: io::Error,
    },

    /// The workspace configuration is not valid TOML or has an invalid shape.
    #[error("could not parse {path}: {source}")]
    ConfigParse {
        /// Configuration path.
        path: PathBuf,
        /// Underlying parse error.
        source: toml::de::Error,
    },

    /// A configuration value is well-formed but not supported.
    #[error("invalid config: {0}")]
    ConfigInvalid(String),

    /// The configured vault path does not exist.
    #[error("vault path does not exist: {path}")]
    VaultMissing {
        /// Resolved vault path.
        path: PathBuf,
    },

    /// The configured vault is not inside the workspace (ADR 0024): the store
    /// only describes content it can address with a workspace-relative path.
    #[error("vault {vault} is outside workspace {workspace}")]
    VaultOutsideWorkspace {
        /// Resolved vault path.
        vault: PathBuf,
        /// Workspace root it must live in.
        workspace: PathBuf,
    },

    /// Markdown ingestion failed at batch level.
    #[error(transparent)]
    Ingest(#[from] IngestError),

    /// The retrieval engine failed at batch level.
    #[error(transparent)]
    Retrieve(#[from] RetrieveError),

    /// A filesystem operation outside per-file handling failed.
    #[error("io error: {source}")]
    Io {
        /// Underlying IO error.
        source: io::Error,
    },

    /// An internal invariant broke: a bug in Momotaro, never a state the user's
    /// workspace can cause. Saying so beats borrowing `io error`'s costume.
    #[error("invariant violated: {0}")]
    Invariant(&'static str),

    /// A derived directory could not be removed, and the path is the only way a
    /// user can act on it.
    #[error(
        "cannot remove {path:?}: {source}; it is a derived directory, so deleting it by hand is always safe"
    )]
    DiscardFailed {
        /// The directory that would not go.
        path: PathBuf,
        /// Underlying IO error.
        source: io::Error,
    },

    /// The search index has never been built for this workspace.
    #[error("search index not built; run `momotaro index` first: {0}")]
    IndexNotBuilt(String),

    /// The derived index is there but unusable, and repairing it is a different
    /// command than ingesting new files.
    #[error("index cannot be used: {0}")]
    IndexUnusable(String),

    /// Whatever sits where a rebuild builds its replacement is not a directory.
    #[error(
        "cannot build a replacement index: {path:?} is in the way; move or remove it and run the command again"
    )]
    StagingBlocked {
        /// The path that is occupied.
        path: PathBuf,
    },

    /// The store is not usable, and the reason belongs to the store — not to the
    /// index, which may not even exist yet.
    #[error("workspace is not ready: {0}")]
    StoreUnhealthy(String),

    /// The local store failed.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// No filesystem backend would start (D40 §4.1), or the vault sits on a
    /// network volume that D40 §4.8 refuses to pretend to support.
    #[error("cannot watch the vault: {reason}")]
    WatcherUnavailable {
        /// What the platform said, already carrying any network-volume caveat.
        reason: String,
    },

    /// The frozen RunSpec was refused (`architecture.md` §19: 校验失败有明确
    /// 错误).
    #[error(transparent)]
    SpecInvalid(#[from] momotaro_contracts::RunSpecError),

    /// The active policy forbids the requested work (e.g. `ask` under
    /// `--no-llm`).
    #[error("policy refused the request: {0}")]
    PolicyRefused(&'static str),

    /// The run a read named does not exist.
    #[error("no run {run_id}")]
    RunNotFound {
        /// The run id the caller asked for.
        run_id: String,
    },

    /// This build carries no production provider transport (`momotaro-llm`'s
    /// seam: "production wires a real transport later"). The engine is proven
    /// end to end by the fake-LLM integration suite; wiring the real
    /// transport is a registered dependency decision, not a silent gap.
    #[error("no provider transport in this build: {0}")]
    ProviderUnavailable(&'static str),
}

#[derive(Debug, Deserialize)]
struct WorkspaceConfig {
    vault: VaultConfig,
    #[serde(default)]
    index: IndexPolicy,
    #[serde(default)]
    model: ModelConfig,
    #[serde(default)]
    policy: PolicySection,
}

/// The `[model]` section (architecture §7 — `momotaro.toml`): the answer
/// slot's endpoint description. Only what this build consumes is typed here;
/// pricing and slot matrices arrive with their own consumers.
#[derive(Debug, Default, Deserialize)]
struct ModelConfig {
    /// The protocol the endpoint speaks (recorded on the run row).
    #[serde(default)]
    provider: Option<String>,
    /// The provider model id (the `LlmRequest` model).
    #[serde(default)]
    model_id: Option<String>,
    /// The provider's context window, when known.
    #[serde(default)]
    context_window: Option<u64>,
    /// Output ceiling handed to the provider.
    #[serde(default = "default_max_output_tokens")]
    max_output_tokens: u32,
    /// The thinking tier (`off | low | medium | high`, or `auto` for the
    /// slot's default — D26).
    #[serde(default = "default_thinking")]
    thinking: String,
}

/// The `[policy]` section (architecture §7): Beta 保守默认 — network and
/// file writes are off unless explicitly allowed.
#[derive(Debug, Deserialize)]
struct PolicySection {
    /// Whether tools may touch the network.
    #[serde(default)]
    allow_network: bool,
    /// Whether tools may write files.
    #[serde(default)]
    allow_write: bool,
    /// The context budget.
    #[serde(default = "default_max_context_tokens")]
    max_context_tokens: u32,
}

impl Default for PolicySection {
    fn default() -> Self {
        Self {
            allow_network: false,
            allow_write: false,
            max_context_tokens: default_max_context_tokens(),
        }
    }
}

fn default_max_output_tokens() -> u32 {
    4096
}

fn default_thinking() -> String {
    "auto".to_owned()
}

fn default_max_context_tokens() -> u32 {
    8192
}

impl WorkspaceConfig {
    /// The model id an ask runs on; refusing without one is a config error,
    /// not a default.
    fn model_id(&self) -> Result<&str, RunError> {
        self.model
            .model_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                RunError::ConfigInvalid("[model].model_id is required to run ask".to_owned())
            })
    }

    /// The thinking tier frozen into the RunSpec (D26: `auto` = the answer
    /// slot's default, `medium`).
    fn model_thinking(&self) -> Result<momotaro_contracts::ThinkingTier, RunError> {
        use momotaro_contracts::ThinkingTier;
        match self.model.thinking.as_str() {
            "auto" => Ok(ThinkingTier::Medium),
            "off" => Ok(ThinkingTier::Off),
            "low" => Ok(ThinkingTier::Low),
            "medium" => Ok(ThinkingTier::Medium),
            "high" => Ok(ThinkingTier::High),
            other => Err(RunError::ConfigInvalid(format!(
                "[model].thinking {other:?} is not a semantic tier; use off | low | medium | high | auto"
            ))),
        }
    }
}

fn default_vault_name() -> String {
    "default".to_owned()
}

#[derive(Debug, Deserialize)]
struct VaultConfig {
    path: PathBuf,
    #[serde(default = "default_vault_name")]
    name: String,
}

/// Validates a configured [`IndexPolicy`] against the capabilities this
/// build actually implements (the D14/D16 seams).
fn validate_policy(policy: &IndexPolicy) -> Result<(), RunError> {
    if policy.lexical != "tantivy_bm25" {
        return Err(RunError::ConfigInvalid(format!(
            "index.lexical {:?} is not supported; only \"tantivy_bm25\"",
            policy.lexical
        )));
    }
    if policy.hybrid {
        return Err(RunError::ConfigInvalid(
            "index.hybrid must be false until a dense baseline exists".to_owned(),
        ));
    }
    if policy.cjk_tokenizer != "cjk" {
        return Err(RunError::ConfigInvalid(format!(
            "index.cjk_tokenizer {:?} is not supported; only \"cjk\"",
            policy.cjk_tokenizer
        )));
    }
    if policy.max_chunk_tokens < 2 * policy.chunk_overlap_tokens {
        return Err(RunError::ConfigInvalid(format!(
            "index.max_chunk_tokens ({}) must be at least twice index.chunk_overlap_tokens ({})",
            policy.max_chunk_tokens, policy.chunk_overlap_tokens
        )));
    }
    Ok(())
}

pub(crate) fn config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_FILE)
}

fn data_dir(root: &Path) -> PathBuf {
    root.join(DATA_DIR)
}

pub(crate) fn database_path(root: &Path) -> PathBuf {
    data_dir(root).join(DATABASE_FILE)
}

pub(crate) fn load_config(root: &Path) -> Result<WorkspaceConfig, RunError> {
    let path = config_path(root);
    let source = fs::read_to_string(&path).map_err(|source| RunError::ConfigRead {
        path: path.clone(),
        source,
    })?;
    toml::from_str(&source).map_err(|source| RunError::ConfigParse { path, source })
}

/// Resolves the vault scope and proves it lives inside the workspace
/// (ADR 0024).
///
/// `vault == workspace root` is allowed — ADR 0019 presumes the data directory
/// may sit inside the vault — but anything else outside is refused, because
/// every stored path is workspace-relative and must not be able to point away
/// from the store that describes it. Containment is decided between
/// canonicalized paths, never lexically (see `momotaro_ingest::is_within`).
fn vault_scope(root: &Path, config: &WorkspaceConfig) -> Result<VaultScope, RunError> {
    let path = if config.vault.path.is_absolute() {
        config.vault.path.clone()
    } else {
        root.join(&config.vault.path)
    };

    if !momotaro_ingest::is_within(root, &path)? {
        return Err(RunError::VaultOutsideWorkspace {
            vault: path,
            workspace: root.to_path_buf(),
        });
    }

    Ok(VaultScope {
        root: path,
        name: config.vault.name.clone(),
    })
}

/// Wall-clock seconds on the caller's clock — the `created_at` stamp the run
/// layer hands the store (like `ingested_at`, the caller owns its clock).
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// A timestamp id that reads as what it is: the millisecond is for a human,
/// the nanosecond half separates two ids inside the same millisecond.
///
/// Unique without a new dependency. One implementation for every id the run
/// layer mints (batches, runs, retrievals) — the batch-id stamp used to be
/// spelled twice (watch and index), which is the drift this closes.
pub(crate) fn stamp_id(prefix: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{prefix}-{}-{:x}", stamp.as_millis(), stamp.subsec_nanos())
}

/// Opens the store the run engine writes to, asking the same health question
/// every run-side entry asks (the store discipline's read-side rule: reuse
/// `store_fault`, do not fork a second open+health branch).
///
/// A database that does not exist yet is not created here — building the
/// store is `init`'s job, and the advice says so (the runnable-advice rule).
pub(crate) fn open_run_store(root: &Path) -> Result<momotaro_store::Store, RunError> {
    match index_service::store_file(root) {
        index_service::StoreFile::Present => {}
        index_service::StoreFile::Absent | index_service::StoreFile::Empty => {
            return Err(RunError::StoreUnhealthy(
                "this workspace is not initialized, run `momotaro init` first".to_owned(),
            ));
        }
    }
    if let Some(reason) = index_service::store_fault(root) {
        return Err(RunError::StoreUnhealthy(reason));
    }
    let mut store = momotaro_store::Store::open(database_path(root))?;
    store.init_schema()?;
    Ok(store)
}

/// Initializes the local SQLite store and returns a small stats report.
pub fn init_workspace(root: impl AsRef<Path>) -> Result<StatsReport, RunError> {
    let root = root.as_ref();
    let config = load_config(root)?;
    let vault = vault_scope(root, &config)?;

    if !vault.root.is_dir() {
        return Err(RunError::VaultMissing { path: vault.root });
    }

    let data_dir = data_dir(root);
    fs::create_dir_all(&data_dir).map_err(|source| RunError::Io { source })?;

    let database_path = database_path(root);
    let mut store = Store::open(&database_path)?;
    store.init_schema()?;
    Ok(store.stats()?)
}

/// Returns non-mutating workspace health.
pub fn doctor(root: impl AsRef<Path>) -> Result<HealthReport, RunError> {
    let sqlite = momotaro_store::sqlite_facts()?;
    doctor_with_facts(root.as_ref(), &sqlite)
}

/// [`doctor`] with the engine facts supplied.
///
/// Split out so the refusal paths can be exercised for engines that this build
/// does not link — an untestable hard failure is a hard failure nobody checks.
pub fn doctor_with_facts(root: &Path, sqlite: &SqliteFacts) -> Result<HealthReport, RunError> {
    // Every branch reports the engine facts: the version of the engine that
    // writes the database is part of the workspace's health, not a detail.
    let network_volume = network_volume_of(root);
    let report = |status: WorkspaceStatus,
                  schema_version: Option<SchemaVersion>,
                  message: Option<String>| HealthReport {
        status,
        schema_version,
        sqlite: Some(sqlite.clone()),
        message,
        network_volume,
    };

    // An engine that must not write this workspace invalidates every other
    // answer, so it is decided before anything about the workspace is read.
    if let Some(message) = engine_refusal_message(sqlite) {
        return Ok(report(WorkspaceStatus::Invalid, None, Some(message)));
    }

    let config = match load_config(root) {
        Ok(config) => config,
        Err(RunError::ConfigRead { path, .. }) => {
            return Ok(report(
                WorkspaceStatus::Invalid,
                None,
                Some(format!("configuration file is missing: {}", path.display())),
            ));
        }
        Err(RunError::ConfigParse { source, .. }) => {
            return Ok(report(
                WorkspaceStatus::Invalid,
                None,
                Some(format!("configuration file is invalid: {source}")),
            ));
        }
        Err(error) => return Err(error),
    };

    let vault = match vault_scope(root, &config) {
        Ok(vault) => vault,
        Err(error) => {
            return Ok(report(
                WorkspaceStatus::Invalid,
                None,
                Some(error.to_string()),
            ));
        }
    };
    if !vault.root.is_dir() {
        return Ok(report(
            WorkspaceStatus::Invalid,
            None,
            Some(format!(
                "vault path does not exist: {}",
                vault.root.display()
            )),
        ));
    }

    let database_path = database_path(root);
    if !database_path.exists() {
        return Ok(report(
            WorkspaceStatus::Uninitialized,
            None,
            Some(format!(
                "database is not initialized: {}",
                database_path.display()
            )),
        ));
    }

    let health = Store::open(database_path)?.health()?;

    // The index is derived and can go stale independently of the store — and a
    // mismatched index makes every retrieval command fail, so a "ready"
    // workspace that cannot search would be a lie.
    if health.status == WorkspaceStatus::Ready
        && let Some(reason) = index_unusable_message(root)
    {
        return Ok(report(
            WorkspaceStatus::Invalid,
            health.schema_version,
            Some(reason),
        ));
    }

    Ok(HealthReport {
        sqlite: Some(sqlite.clone()),
        // The store has no workspace, so it answered `None`; this surface
        // does know the workspace and answers for it (D31 / D40 §4.8).
        // The closure above already asked; asking again is a config parse and
        // up to nine Win32 calls for a value that cannot have moved.
        network_volume,
        ..health
    })
}

/// The refusal message for an engine that must not write this workspace.
///
/// Two distinct reasons, with distinct advice — and neither ever names the
/// withdrawn 3.52 series as the way out.
fn engine_refusal_message(facts: &SqliteFacts) -> Option<String> {
    if facts.withdrawn_release {
        return Some(format!(
            "bundled SQLite {} is part of the withdrawn 3.52 series, which misreports \
             integrity_check corruption; upgrade rusqlite to a release bundling 3.51.3 or newer",
            facts.version
        ));
    }

    facts.wal_reset_window.then(|| {
        format!(
            "bundled SQLite {} sits in the WAL-reset window (3.7.0\u{2013}3.51.2, fixed in 3.51.3) \
             and can corrupt a WAL database; upgrade rusqlite to a release bundling 3.51.3 or newer",
            facts.version
        )
    })
}

/// Returns current local store stats.
pub fn stats(root: impl AsRef<Path>) -> Result<StatsReport, RunError> {
    let database_path = database_path(root.as_ref());

    if !database_path.exists() {
        return Ok(StatsReport {
            status: WorkspaceStatus::Uninitialized,
            schema_version: None,
            database_path,
            initialized: false,
            sources: None,
            chunks: None,
        });
    }

    Ok(Store::open(database_path)?.stats()?)
}

/// Renders the human one-line summary of an index run.
///
/// Kept here rather than in the CLI so it is unit-testable on every platform,
/// and so the collision count is stated once: collisions stay exit-code
/// neutral, so this line plus the JSON `kind` field are how a caller notices
/// them.
pub fn format_index_summary(report: &IndexReport) -> String {
    let collisions = report
        .errors
        .iter()
        .filter(|error| error.kind == IndexFileErrorKind::KeyCollision)
        .count();
    format!(
        "indexed {} of {} files ({} new revisions, {} skipped), {} chunks, {} errors ({} key collisions) in {} ms",
        report.files_indexed,
        report.files_scanned,
        report.revisions_created,
        report.files_skipped,
        report.chunks_written,
        report.errors.len(),
        collisions,
        report.duration_ms
    )
}

/// Longest excerpt printed on the excerpt line, in characters.
const EXCERPT_LINE_CHARS: usize = 100;

/// Removes characters that could drive or spoof a printed line.
///
/// Every field this module renders carries text the program did not author
/// (file names on POSIX, note bodies, `paper`/`web` content), so one rule
/// covers them all: line breaks become spaces (the line stays readable) and
/// unprintable characters are dropped — C0/C1 controls can drive the terminal,
/// and the bidi / zero-width format characters can hide or reorder the rest of
/// the line.
pub(crate) fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| !is_unprintable(*c))
        .collect()
}

/// Whether `c` must never reach a terminal: C0/C1 controls plus the format
/// characters that reorder or hide text (`U+202E` right-to-left override,
/// `U+200B` zero width space, and their neighbours).
fn is_unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'                  // SOFT HYPHEN
                | '\u{061c}'            // ARABIC LETTER MARK
                | '\u{180e}'            // MONGOLIAN VOWEL SEPARATOR
                | '\u{200b}'..='\u{200f}' // ZWSP, ZWNJ, ZWJ, LRM, RLM
                | '\u{202a}'..='\u{202e}' // bidi embedding / override
                | '\u{2060}'..='\u{206f}' // word joiner .. invisible operators
                | '\u{feff}'            // BOM / zero width no-break space
                | '\u{e0000}'..='\u{e007f}' // tag characters
        )
}

/// Renders the excerpt line under one search hit.
///
/// Excerpts are indexed content — including `paper` and `web` material that is
/// not user-authored — so the text goes through `sanitize` (private, but the
/// reason is the reader's: control characters must not reach a terminal) and is
/// cut to `EXCERPT_LINE_CHARS`.
pub fn format_search_excerpt(excerpt: &str) -> String {
    let cleaned = sanitize(excerpt);
    if cleaned.chars().count() <= EXCERPT_LINE_CHARS {
        return cleaned;
    }
    let truncated: String = cleaned.chars().take(EXCERPT_LINE_CHARS).collect();
    format!("{truncated}...")
}

/// Renders one rebuild report as a terminal line.
///
/// A replaced stale index is said out loud: throwing away a derived artifact is
/// fine, doing it silently is how "my search results changed" becomes a mystery.
pub fn format_rebuild(report: &RebuildReport) -> String {
    match &report.replaced_stale {
        Some(reason) => format!(
            "rebuilt index over {} chunks in {} ms (discarded an unusable index: {})",
            report.chunks,
            report.duration_ms,
            sanitize(reason)
        ),
        None => format!(
            "rebuilt index over {} chunks in {} ms",
            report.chunks, report.duration_ms
        ),
    }
}

/// Renders one command failure as a single escaped terminal line.
///
/// Errors carry paths and keys straight from the filesystem; the JSON form is
/// escaped by its serializer, the terminal is not.
pub fn format_error(error: &RunError) -> String {
    sanitize(&error.to_string())
}

/// Whether a health report says the workspace is usable.
///
/// `doctor` gates its exit code on this: a health check that cannot fail a
/// script is not a check (docs/docs/usage.md, "Exit codes").
pub fn is_healthy(report: &HealthReport) -> bool {
    report.status == WorkspaceStatus::Ready
}

/// Whether the report says this workspace has never been initialized.
///
/// A caller uses it to give the right advice: a workspace nothing has run in
/// needs `init`, and telling its owner to run `index` would send them to a
/// command that cannot succeed.
pub fn is_uninitialized(report: &HealthReport) -> bool {
    report.status == WorkspaceStatus::Uninitialized
}

/// The copy every network-volume warning carries (D31 / D40 §4.8).
///
/// One sentence for both exits: `doctor`'s human line and the watcher's
/// `network_volume` line say the same thing, so a promise one of them makes
/// cannot be missing from the other.
pub const NETWORK_VOLUME_NOTE: &str = "this workspace is on a network volume, which Momotaro does not promise to be real-time; SQLite's WAL does not work over a network filesystem, and broken locking there can corrupt the database";

/// Whether the volumes holding this workspace's data and vault are network
/// mounts (D31 / D40 §4.8).
///
/// Both paths are asked because they can differ: the store may be local while
/// the vault sits on a share, or the other way round. `None` means nothing
/// could be read — never "no".
fn network_volume_of(root: &Path) -> Option<bool> {
    let data = match momotaro_platform::volume_info(&data_dir(root)).remote {
        answer @ Some(_) => answer,
        // Before `init` the data directory does not exist; its parent is on
        // the same volume, and that is the answer the question needs.
        None => momotaro_platform::volume_info(root).remote,
    };
    let vault = load_config(root)
        .ok()
        .and_then(|config| vault_scope(root, &config).ok())
        .and_then(|scope| momotaro_platform::volume_info(&scope.root).remote);
    let answers: Vec<bool> = [data, vault].into_iter().flatten().collect();
    (!answers.is_empty()).then(|| answers.into_iter().any(|remote| remote))
}

/// Renders one workspace health report as a terminal line.
///
/// A report that is not `Ready` must say why: printing "workspace is ready" for
/// an `Invalid` workspace hides the failure from the human who ran `doctor`.
pub fn format_health(report: &HealthReport) -> String {
    let line = match (&report.status, &report.message) {
        (WorkspaceStatus::Ready, _) => "workspace is ready".to_owned(),
        (status, Some(message)) => format!("workspace {status:?}: {}", sanitize(message)),
        (status, None) => format!("workspace {status:?}"),
    };

    // The engine that writes the database is part of the answer, not a detail
    // reserved for `--json`.
    let line = match report.sqlite.as_ref() {
        Some(facts) => format!("{line} (SQLite {})", facts.version),
        None => line,
    };
    // Neither is the mount this workspace lives on: `--json` carries the
    // `network_volume` field, so the human exit has to carry the sentence.
    match report.network_volume {
        Some(true) => format!("{line}; {NETWORK_VOLUME_NOTE}"),
        _ => line,
    }
}

/// Renders one search hit as a terminal line.
///
/// The source key and the title are both name-derived (the title falls back to
/// the file stem), so both are escaped: an ESC or newline in a file name must
/// never reach the terminal.
pub fn format_search_hit(hit: &SearchHit) -> String {
    let title = hit.title.as_deref().unwrap_or("-");
    format!(
        "{}. {:?}#{}  {:?}  score={:.3}",
        hit.hit.rank, hit.hit.source_key, hit.hit.ordinal, title, hit.hit.score
    )
}

/// The human rendering of one stats report.
///
/// Every field the JSON outlet carries is named here (两出口字段一致): a
/// script reading `--json` and a human reading the terminal see the same
/// facts. `unknown` stays `unknown` — a missing count is never `0`
/// (architecture §5.2 rule 12).
pub fn format_stats(report: &StatsReport) -> String {
    let shown = |value: Option<u64>| match value {
        Some(value) => value.to_string(),
        None => "unknown".to_owned(),
    };
    format!(
        "status: {}\nschema version: {}\ndatabase: {}\ninitialized: {}\nsources: {}\nchunks: {}\n",
        report.status.as_str(),
        report
            .schema_version
            .map_or_else(|| "unknown".to_owned(), |version| version.0.to_string()),
        sanitize(&report.database_path.display().to_string()),
        report.initialized,
        shown(report.sources),
        shown(report.chunks),
    )
}

/// The human rendering of one coverage report: the counts, then every failure.
///
/// The key is escaped as a debug string and the code and message are sanitized:
/// all three carry text this program did not author (a `source_key` comes from a
/// file name, and POSIX allows control characters in one), and none of them may
/// drive or spoof the terminal — the same reason [`format_index_error`] does it.
pub fn format_coverage(report: &CoverageReport) -> String {
    let mut line = format!(
        "ingest batch {:?}: {} indexed, {} skipped, {} refreshed, {} failed{}",
        report.batch_id,
        report.indexed,
        report.skipped,
        report.refreshed,
        report.failures.len(),
        // An unfinished batch is exactly what ADR 0026 says this log cannot tell
        // apart: still running, or the process died.
        if report.finished {
            ""
        } else {
            " (no ingest.finished yet: still running, or the process died)"
        },
    );
    // Both vocabularies, one rendering: the state is the rows', never the
    // code's — the same code rides rows that are in the index anyway and rows
    // nothing was read for, so one parenthetical for all of them would
    // contradict the counts printed above it.
    for (code, counts) in report
        .degradations
        .iter()
        .map(|(code, counts)| (code.as_str(), counts))
        .chain(
            report
                .warnings
                .iter()
                .map(|(code, counts)| (code.as_str(), counts)),
        )
    {
        line.push('\n');
        let state = match (counts.in_index, counts.skipped) {
            (in_index, 0) => format!("({in_index} indexed anyway)"),
            (0, skipped) => format!("({skipped} skipped: nothing was read)"),
            (in_index, skipped) => {
                format!("({in_index} indexed anyway, {skipped} skipped)")
            }
        };
        line.push_str(&format!("  {} x {} {}", counts.total(), code, state));
    }
    for failure in &report.failures {
        line.push('\n');
        line.push_str(&format!(
            "  {:?}: {}: {}",
            failure.source_key.as_deref().unwrap_or("<no key>"),
            sanitize(failure.reason_code.as_deref().unwrap_or("unknown")),
            sanitize(failure.message.as_deref().unwrap_or("")),
        ));
    }
    line
}

/// Renders one per-file index failure as a terminal line.
///
/// The key is escaped as a debug string and the message is sanitized: both
/// carry file-derived text (POSIX allows control characters in names), and
/// neither may drive or spoof the terminal.
pub fn format_index_error(error: &IndexFileError) -> String {
    let key = error.source_key.as_deref().unwrap_or("<no key>");
    format!("{key:?}: {}: {}", error.kind, sanitize(&error.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::index_service::{discard, index_dir, staging_dir};

    #[test]
    fn error_lines_escape_the_terminal() {
        // A failure can name a path or a key the program did not author, and one
        // line per failure means it must not be able to move the cursor or hide
        // text: the assertions are on the characters themselves, not on the
        // predicate the implementation happens to filter with.
        let error = RunError::Retrieve(RetrieveError::MissingOrigin(
            "a\u{1b}[31m\u{202e}b.md".to_owned(),
        ));
        let line = format_error(&error);
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        assert!(line.contains("a[31mb.md"), "still readable: {line:?}");
    }

    #[test]
    fn rebuild_lines_report_what_was_replaced() {
        // The ordinary case first: no replacement, no noise.
        let plain = RebuildReport {
            chunks: 18,
            duration_ms: 42,
            replaced_stale: None,
        };
        assert_eq!(
            format_rebuild(&plain),
            "rebuilt index over 18 chunks in 42 ms"
        );

        let rebuilt = RebuildReport {
            chunks: 18,
            duration_ms: 42,
            replaced_stale: Some("missing field `origin_class`".to_owned()),
        };
        assert_eq!(
            format_rebuild(&rebuilt),
            "rebuilt index over 18 chunks in 42 ms (discarded an unusable index: missing field `origin_class`)"
        );

        // The reason is tantivy's, and it can carry file content: the terminal
        // needs the same escaping as every other human-readable line.
        let spoiled = RebuildReport {
            chunks: 1,
            duration_ms: 1,
            replaced_stale: Some("meta.json \u{1b}[31m\u{202e}".to_owned()),
        };
        let line = format_rebuild(&spoiled);
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
    }

    #[test]
    fn an_index_directory_without_an_index_names_the_command_that_repairs_it() {
        let (dir, _vault) = fixture_workspace();
        let before = index_workspace(dir.path(), None).expect("index");
        // The segments are still there, so the directory looks like an index —
        // but `index` only adds files the store has not seen, so telling the user
        // to run it is advice that cannot work.
        fs::remove_file(index_dir(dir.path()).join("meta.json")).expect("drop the meta");

        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("no usable index");
        assert!(
            matches!(&error, RunError::IndexUnusable(reason) if reason.contains("rebuild-index")),
            "names the command that can repair it: {error:?}"
        );
        let error = index_workspace(dir.path(), None).expect_err("index cannot repair it");
        assert!(matches!(error, RunError::IndexUnusable(_)), "{error:?}");
        assert_eq!(
            doctor(dir.path()).expect("doctor").status,
            WorkspaceStatus::Invalid
        );

        // The named command repairs it, and says what it threw away.
        let rebuilt = rebuild_index(dir.path()).expect("rebuild");
        assert_eq!(rebuilt.chunks, before.chunks_written);
        assert!(
            rebuilt.replaced_stale.is_some(),
            "a leftover directory is announced, not silently reused: {rebuilt:?}"
        );
        assert!(
            !search_workspace(dir.path(), "傅里叶", 10)
                .expect("search")
                .hits
                .is_empty(),
            "retrieval is back"
        );
    }

    #[test]
    fn index_summary_states_the_collision_count() {
        // Two failures of different kinds: the collision count must be the
        // collision count, not the total. (A mutation that counted every error
        // survived the earlier version of this test.)
        let report = IndexReport {
            batch_id: "index-1".to_owned(),
            files_scanned: 3,
            files_skipped: 0,
            files_indexed: 2,
            revisions_created: 2,
            chunks_written: 2,
            errors: vec![
                IndexFileError {
                    source_key: Some("note:caf\u{00e9}.md".to_owned()),
                    kind: IndexFileErrorKind::KeyCollision,
                    message: "claimed by both".to_owned(),
                },
                IndexFileError {
                    source_key: Some("note:\u{1f}.md".to_owned()),
                    kind: IndexFileErrorKind::InvalidUtf8,
                    message: "not utf-8".to_owned(),
                },
            ],
            duration_ms: 7,
        };
        let line = format_index_summary(&report);
        assert!(line.contains("2 errors (1 key collisions)"), "{line}");
        assert!(line.contains("in 7 ms"), "{line}");
    }

    #[test]
    fn index_error_without_a_key_renders_a_placeholder() {
        let error = IndexFileError {
            source_key: None,
            kind: IndexFileErrorKind::Other,
            message: "io failure".to_owned(),
        };
        let line = format_index_error(&error);
        assert!(line.contains("<no key>"), "{line}");
        assert!(line.contains("other"), "{line}");
    }

    #[test]
    fn search_excerpt_strips_control_characters() {
        let line = format_search_excerpt("body\u{1b}[2J and\u{7} more");
        assert!(!line.chars().any(is_unprintable), "{line:?}");
        assert!(line.contains("body"), "{line:?}");
    }

    #[test]
    fn search_excerpt_strips_format_characters_that_could_spoof_the_line() {
        // U+202E is a right-to-left override and U+200B a zero width space:
        // either one lets a note reorder or hide the rest of the line.
        let line = format_search_excerpt("safe\u{202e}evil\u{200b}hidden");
        assert!(!line.chars().any(is_unprintable), "{line:?}");
        assert!(line.contains("safe"), "{line:?}");
    }

    #[test]
    fn index_error_lines_sanitize_the_message_too() {
        let error = IndexFileError {
            source_key: Some("note:a.md".to_owned()),
            kind: IndexFileErrorKind::Other,
            message: "io\u{1b}[2J failure".to_owned(),
        };
        let line = format_index_error(&error);
        assert!(!line.chars().any(is_unprintable), "{line:?}");
        assert!(line.contains("io[2J failure"), "{line:?}");
    }

    #[test]
    fn index_error_lines_escape_the_key() {
        let error = IndexFileError {
            source_key: Some("note:caf\u{00e9}\u{1b}[2J.md".to_owned()),
            kind: IndexFileErrorKind::KeyCollision,
            message: format!(
                "claimed by both {0:?} and {1:?}",
                "cafe\u{0301}.md", "caf\u{00e9}.md"
            ),
        };
        let line = format_index_error(&error);
        assert!(!line.contains('\u{1b}'), "raw control character: {line}");
        assert!(line.contains(r"\u{1b}"), "escaped form expected: {line}");
        assert!(
            line.contains("key_collision"),
            "typed kind must be shown: {line}"
        );
        assert!(
            line.contains(r#"cafe\u{301}.md"#),
            "both spellings stay distinguishable: {line}"
        );
    }

    #[test]
    fn search_hit_lines_escape_key_and_title() {
        let hit = SearchHit {
            hit: momotaro_contracts::RetrievalHit {
                rank: 1,
                source_key: "note:a\u{1b}[2J.md".to_owned(),
                revision_hash: "rev".to_owned(),
                chunk_id: "chunk".to_owned(),
                chunk_hash: "hash".to_owned(),
                ordinal: 0,
                heading_path: None,
                score: 1.0,
                locator_json: "{}".to_owned(),
            },
            title: Some("ti\u{1b}tle".to_owned()),
            excerpt: "body".to_owned(),
        };
        let line = format_search_hit(&hit);
        assert!(!line.contains('\u{1b}'), "raw control character: {line}");
        assert!(line.contains(r"\u{1b}"), "escaped form expected: {line}");
    }

    fn write_config(root: &Path, vault: &Path) {
        fs::create_dir_all(root.join(".momotaro")).expect("create data dir");
        fs::write(
            config_path(root),
            format!(
                "vault.path = {:?}\nvault.name = \"default\"\n",
                vault.to_string_lossy()
            ),
        )
        .expect("write config");
    }

    #[test]
    fn a_vault_outside_the_workspace_is_refused() {
        // ADR 0024: every stored path is workspace-relative, so a vault that is
        // not inside the workspace cannot be described by the store at all.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("ws");
        let outside = dir.path().join("outside-vault");
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&outside).expect("create outside vault");
        write_config(&root, &outside);

        let error = init_workspace(&root).expect_err("init must refuse");
        assert!(
            matches!(error, RunError::VaultOutsideWorkspace { .. }),
            "{error}"
        );
        let error = index_workspace(&root, None).expect_err("index must refuse");
        assert!(
            matches!(error, RunError::VaultOutsideWorkspace { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_workspace_root_itself_is_a_valid_vault() {
        // ADR 0019 presumes `vault == workspace root`: the data directory then
        // sits inside the vault and is skipped by the walker as hidden.
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("ws");
        fs::create_dir_all(&root).expect("create root");
        write_config(&root, &root);
        fs::write(root.join("a.md"), "# A\n\nbody\n").expect("write note");

        init_workspace(&root).expect("init accepts vault == root");
        let report = index_workspace(&root, None).expect("index accepts vault == root");
        // The config file sits in the vault here, so it is a vault file: the walker
        // registers it as an attachment rather than pretending it is not there
        // (§5 第 10 条). One note, one registered file, one chunk.
        assert_eq!(report.files_scanned, 2, "{report:?}");
        assert_eq!(report.files_indexed, 2, "{report:?}");
        assert_eq!(report.chunks_written, 1, "{report:?}");
        assert_eq!(report.errors.len(), 0, "{report:?}");
        let coverage = coverage_report(&root).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::AttachmentUnindexed)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "the config file is registered, and the report says so: {coverage:?}"
        );

        let health = doctor(&root).expect("doctor accepts vault == root");
        assert_eq!(health.status, WorkspaceStatus::Ready, "{health:?}");
    }

    fn parse_index_policy(index_section: &str) -> IndexPolicy {
        let full = format!("vault.path = \"v\"\n{index_section}");
        toml::from_str::<WorkspaceConfig>(&full)
            .expect("parse config")
            .index
    }

    #[test]
    fn missing_config_is_reported() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let error = load_config(dir.path()).expect_err("missing config must fail");
        assert!(matches!(error, RunError::ConfigRead { .. }));
    }

    #[test]
    fn missing_vault_is_reported_by_doctor() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let vault = dir.path().join("missing-vault");
        write_config(dir.path(), &vault);

        let report = doctor(dir.path()).expect("doctor succeeds");
        assert_eq!(report.status, WorkspaceStatus::Invalid);
        assert!(report.message.expect("doctor message").contains("vault"));
    }

    #[test]
    fn init_creates_ready_store() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let vault = dir.path().join("notes");
        fs::create_dir_all(&vault).expect("create vault");
        write_config(dir.path(), &vault);

        let report = init_workspace(dir.path()).expect("initialize workspace");
        assert_eq!(report.status, WorkspaceStatus::Ready);
        assert!(report.initialized);
        assert!(database_path(dir.path()).exists());
    }

    #[test]
    fn config_without_index_section_uses_defaults() {
        let policy = parse_index_policy("");
        assert_eq!(policy, IndexPolicy::default());
        validate_policy(&policy).expect("defaults validate");
    }

    #[test]
    fn index_config_overrides_apply() {
        let policy = parse_index_policy(
            "[index]\nlexical = \"tantivy_bm25\"\nmax_chunk_tokens = 256\nchunk_overlap_tokens = 32\n",
        );
        assert_eq!(policy.max_chunk_tokens, 256);
        assert_eq!(policy.chunk_overlap_tokens, 32);
        validate_policy(&policy).expect("overrides validate");
    }

    #[test]
    fn validate_policy_rejects_unknown_lexical() {
        let policy = parse_index_policy("[index]\nlexical = \"lunr\"\n");
        assert!(matches!(
            validate_policy(&policy),
            Err(RunError::ConfigInvalid(_))
        ));
    }

    #[test]
    fn validate_policy_rejects_hybrid() {
        let policy = parse_index_policy("[index]\nhybrid = true\n");
        assert!(matches!(
            validate_policy(&policy),
            Err(RunError::ConfigInvalid(_))
        ));
    }

    #[test]
    fn validate_policy_rejects_unknown_tokenizer() {
        let policy = parse_index_policy("[index]\ncjk_tokenizer = \"jieba\"\n");
        assert!(matches!(
            validate_policy(&policy),
            Err(RunError::ConfigInvalid(_))
        ));
    }

    #[test]
    fn validate_policy_rejects_overlap_at_or_above_half_budget() {
        // Exactly half the budget is the valid boundary (max >= 2*overlap).
        let equal =
            parse_index_policy("[index]\nmax_chunk_tokens = 128\nchunk_overlap_tokens = 64\n");
        assert!(validate_policy(&equal).is_ok());

        let over =
            parse_index_policy("[index]\nmax_chunk_tokens = 100\nchunk_overlap_tokens = 64\n");
        assert!(matches!(
            validate_policy(&over),
            Err(RunError::ConfigInvalid(_))
        ));

        let just_under_ok =
            parse_index_policy("[index]\nmax_chunk_tokens = 129\nchunk_overlap_tokens = 64\n");
        assert!(validate_policy(&just_under_ok).is_ok());
    }

    #[test]
    fn stats_fills_sources_and_chunks_when_initialized() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let vault = dir.path().join("notes");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\n傅里叶标记 body\n").expect("write note");
        write_config(dir.path(), &vault);

        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");
        assert_eq!(report.revisions_created, 1);

        let report = stats(dir.path()).expect("stats");
        assert_eq!(report.sources, Some(1));
        assert!(report.chunks.unwrap_or(0) >= 1);

        let uninitialized = stats(tempfile::tempdir().expect("empty").path()).expect("stats");
        assert_eq!(uninitialized.sources, None);
        assert_eq!(uninitialized.chunks, None);
    }

    #[test]
    fn index_rejects_unsupported_policy_from_config_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let vault = dir.path().join("notes");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(
            config_path(dir.path()),
            "vault.path = \"notes\"\n\n[index]\nhybrid = true\n",
        )
        .expect("write config");

        let error = index_workspace(dir.path(), None).expect_err("hybrid must fail");
        assert!(matches!(error, RunError::ConfigInvalid(_)));
    }

    /// Copies the checked-in fixture vault into a temp workspace and writes
    /// a config pointing at the copy, so mutations never touch fixtures.
    fn fixture_workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let vault = dir.path().join("vault");
        fs_extra_copy_dir(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vault"),
            &vault,
        );
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        (dir, vault)
    }

    fn fs_extra_copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("create vault copy");
        for entry in fs::read_dir(from).expect("read fixture dir") {
            let entry = entry.expect("fixture entry");
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                fs_extra_copy_dir(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).expect("copy fixture file");
            }
        }
    }

    #[test]
    fn stored_local_path_is_workspace_relative() {
        let (dir, _vault) = fixture_workspace();
        let report = index_workspace(dir.path(), None).expect("index");
        assert_eq!(report.revisions_created, 5, "{report:?}");

        let store = store_of(dir.path());
        let revision = store
            .current_revision("note:fourier.md")
            .expect("read")
            .expect("exists");
        let local_path = revision.local_path.expect("local_path recorded");
        assert!(
            !Path::new(&local_path).is_absolute(),
            "local_path is workspace-relative, got {local_path}"
        );
        assert_eq!(local_path, "vault/fourier.md");
        assert!(!local_path.contains('\\'), "paths use `/`: {local_path}");
        assert_eq!(revision.raw_name.as_deref(), Some("fourier.md"));
    }

    #[test]
    fn a_moved_vault_refreshes_the_stored_path_without_touching_bytes() {
        let (dir, vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        // Relocate the vault and repoint the configuration: same keys, same
        // bytes, same mtime (a rename preserves it). This is the shape whose
        // stored path used to stay stale forever — the fingerprint matched, so
        // the file was skipped before its path was ever compared.
        let moved = dir.path().join("notes").join("vault");
        fs::create_dir_all(moved.parent().expect("parent path")).expect("create parent");
        fs::rename(&vault, &moved).expect("move the vault");
        write_config(dir.path(), &moved);

        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(report.files_skipped, 0, "a moved path is not up to date");
        assert_eq!(report.files_indexed, 5, "{report:?}");
        assert_eq!(report.revisions_created, 0, "the bytes did not change");

        let store = store_of(dir.path());
        let revision = store
            .current_revision("note:fourier.md")
            .expect("read")
            .expect("exists");
        assert_eq!(
            revision.local_path.as_deref(),
            Some("notes/vault/fourier.md"),
            "the stored path follows the move"
        );

        // The move is a coverage row too (ADR 0026): a path-only refresh is
        // reported as `metadata_refreshed`, not silently folded into the counts.
        let coverage: Vec<_> = store
            .store_events_for_batch(&report.batch_id)
            .expect("read batch")
            .into_iter()
            .filter(|event| event.event_type == "ingest.coverage")
            .collect();
        assert_eq!(coverage.len(), 5, "one row per file, five files");
        assert!(
            coverage
                .iter()
                .all(|event| event.outcome.as_deref() == Some("metadata_refreshed")),
            "a moved path is a metadata refresh, not a re-index: {coverage:?}"
        );
        assert!(
            coverage.iter().all(|event| event.reason_code.is_none()),
            "a refresh is not a failure"
        );

        // …and the reader counts them as refreshes, not as new work.
        let summary = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(summary.batch_id, report.batch_id);
        assert_eq!(summary.refreshed, 5, "{summary:?}");
        assert_eq!(summary.indexed, 0, "{summary:?}");
        assert!(summary.failures.is_empty(), "{summary:?}");
    }

    #[test]
    fn an_index_run_is_one_batch_with_one_row_per_file() {
        // ADR 0026 through the public API: the batch has a beginning, one row
        // per file and an end, and the report names the same batch.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nfirst\n").expect("write");
        fs::write(vault.join("b.md"), "# B\n\nsecond\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");

        let report = index_workspace(dir.path(), None).expect("index");
        let store = store_of(dir.path());
        let events = store
            .store_events_for_batch(&report.batch_id)
            .expect("read batch");

        assert_eq!(events.len(), 4, "started + two files + finished");
        assert_eq!(events[0].event_type, "ingest.started");
        assert_eq!(events[events.len() - 1].event_type, "ingest.finished");
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "one batch, one sequence"
        );

        let coverage: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == "ingest.coverage")
            .collect();
        assert_eq!(coverage.len(), 2, "one row per file");

        // The pairing is the point of an audit row: which file, and what
        // happened to it. A row naming the wrong source is the one failure this
        // table must not have, so the pairs are asserted — not just the keys.
        let mut pairs: Vec<(&str, &str)> = coverage
            .iter()
            .map(|event| {
                (
                    event.source_key.as_deref().expect("a row names its source"),
                    event.outcome.as_deref().expect("a row says what happened"),
                )
            })
            .collect();
        pairs.sort_unstable();
        assert_eq!(
            pairs,
            vec![("note:a.md", "indexed"), ("note:b.md", "indexed")],
            "each row names the file it is about"
        );
        assert!(
            coverage.iter().all(|event| event.reason_code.is_none()),
            "a successful file gets no failure code"
        );
        assert!(
            events.iter().all(|event| event.domain == "ingest"),
            "every ingest row says so"
        );
        assert!(
            events
                .iter()
                .all(|event| event.occurred_at > 0 && event.recorded_at >= event.occurred_at),
            "the write moment is the store's clock, not the event's"
        );
        assert!(
            events.iter().all(|event| event.external_id.is_none()),
            "ingest has no provider ids"
        );

        // `ingest.finished` carries the report's counters — that payload is the
        // reason the event has one at all.
        let payload: serde_json::Value =
            serde_json::from_str(&events[events.len() - 1].data_json).expect("finished is JSON");
        assert_eq!(payload["files_scanned"], 2, "{payload}");
        assert_eq!(payload["files_indexed"], 2, "{payload}");
        assert_eq!(payload["errors"], 0, "{payload}");

        assert!(
            events.iter().all(|event| event.batch_id == report.batch_id),
            "the report and the rows agree on the batch"
        );

        // Write order, not only `seq`: a mutant that appends `started` after the
        // loop keeps the seq vector identical, so the batch is read back through
        // insertion order (`latest_store_events` returns newest first) and its
        // first row must be the beginning.
        let by_id: Vec<String> = store
            .latest_store_events(100)
            .expect("read latest")
            .into_iter()
            .filter(|event| event.batch_id == report.batch_id)
            .map(|event| event.event_type)
            .rev()
            .collect();
        assert_eq!(
            by_id.first().map(String::as_str),
            Some("ingest.started"),
            "the first row written is the batch's beginning: {by_id:?}"
        );
        assert_eq!(
            by_id.last().map(String::as_str),
            Some("ingest.finished"),
            "…and the last is its end"
        );
    }

    #[test]
    fn a_second_index_run_reports_its_files_as_skipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let first = index_workspace(dir.path(), None).expect("first index");

        let report = index_workspace(dir.path(), None).expect("second index");
        let store = store_of(dir.path());
        let coverage: Vec<_> = store
            .store_events_for_batch(&report.batch_id)
            .expect("read batch")
            .into_iter()
            .filter(|event| event.event_type == "ingest.coverage")
            .collect();

        assert_eq!(coverage.len(), 1);
        assert_eq!(
            coverage[0].outcome.as_deref(),
            Some("skipped"),
            "an unchanged file is covered, not silently dropped"
        );
        assert!(coverage[0].reason_code.is_none(), "no failure to explain");
        assert_ne!(first.batch_id, report.batch_id, "two runs are two batches");
        assert!(
            report.batch_id.starts_with("index-"),
            "the shape the ADR documents: {}",
            report.batch_id
        );
    }

    #[test]
    fn an_empty_vault_still_leaves_a_beginning_and_an_end() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");

        let report = index_workspace(dir.path(), None).expect("index");
        let store = store_of(dir.path());
        let events = store
            .store_events_for_batch(&report.batch_id)
            .expect("read batch");

        let types: Vec<&str> = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec!["ingest.started", "ingest.finished"],
            "no files, no coverage rows"
        );
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(report.files_scanned, 0);
    }

    #[test]
    fn a_collision_gets_an_error_row_with_a_stable_code() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("caf\u{00e9}.md"), "# A\n\nnfc\n").expect("write");
        fs::write(vault.join("cafe\u{0301}.md"), "# B\n\nnfd\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");

        let report = index_workspace(dir.path(), None).expect("index");
        let store = store_of(dir.path());
        let coverage: Vec<_> = store
            .store_events_for_batch(&report.batch_id)
            .expect("read batch")
            .into_iter()
            .filter(|event| event.event_type == "ingest.coverage")
            .collect();

        assert_eq!(
            coverage.len(),
            2,
            "both files get a row, one of them an error"
        );
        let error = coverage
            .iter()
            .find(|event| event.outcome.as_deref() == Some("error"))
            .expect("the refused file is an error row");
        assert_eq!(
            error.reason_code.as_deref(),
            Some("key_collision"),
            "the code is the typed kind's spelling, not a message"
        );
        assert!(
            error
                .message
                .as_deref()
                .is_some_and(|text| text.contains("caf")),
            "the row keeps the detail: {:?}",
            error.message
        );
    }

    #[test]
    fn the_last_batch_can_be_read_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");

        assert!(
            coverage_report(dir.path()).expect("read").is_none(),
            "nothing indexed yet is not a batch"
        );

        let report = index_workspace(dir.path(), None).expect("index");
        let coverage = coverage_report(dir.path())
            .expect("read")
            .expect("a batch exists");
        assert_eq!(coverage.batch_id, report.batch_id);
        assert_eq!(coverage.indexed, 1);
        assert_eq!(coverage.skipped, 0);
        assert_eq!(coverage.refreshed, 0);
        assert!(coverage.failures.is_empty(), "{:?}", coverage.failures);
        assert_eq!(coverage.events, 3, "started + one file + finished");
        assert!(coverage.finished);

        // The next run reports itself, not the one before it.
        let second = index_workspace(dir.path(), None).expect("second index");
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(coverage.batch_id, second.batch_id);
        assert_eq!(coverage.skipped, 1, "the file did not change");
        let line = format_coverage(&coverage);
        assert!(line.contains(&second.batch_id), "{line}");
        assert!(line.contains("1 skipped"), "{line}");
    }

    #[test]
    fn a_refused_file_shows_up_as_a_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("caf\u{00e9}.md"), "# A\n\nnfc\n").expect("write");
        fs::write(vault.join("cafe\u{0301}.md"), "# B\n\nnfd\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(coverage.indexed, 1, "first writer wins");
        assert_eq!(coverage.failures.len(), 1, "{:?}", coverage.failures);
        assert_eq!(
            coverage.failures[0].reason_code.as_deref(),
            Some("key_collision"),
            "the failure keeps the stable code, not just the text"
        );
        let line = format_coverage(&coverage);
        assert!(line.contains("key_collision"), "{line}");
        assert!(line.contains("1 failed"), "{line}");
    }

    #[test]
    fn an_unreadable_outcome_is_reported_not_dropped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        // A row whose outcome this build does not know: the reader must say so
        // rather than skip the row (an unexplained row is a fact about the
        // batch).
        let mut store = store_of(dir.path());
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 1,
                batch_id: &report.batch_id,
                seq: 99,
                domain: "ingest",
                event_type: "ingest.coverage",
                source_key: Some("note:from-the-future.md"),
                outcome: Some("reticulated"),
                reason_code: None,
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append a foreign row");

        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(coverage.failures.len(), 1, "{:?}", coverage.failures);
        assert_eq!(
            coverage.failures[0].source_key.as_deref(),
            Some("note:from-the-future.md"),
            "the row is reported, with what it carried"
        );
        assert_eq!(
            coverage.failures[0].reason_code.as_deref(),
            None,
            "the row had no code; the reader does not invent one"
        );
    }

    #[test]
    fn a_batch_without_its_end_is_reported_as_unfinished() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        // What an aborted run leaves behind: a beginning with no end. The reader
        // must not call it complete.
        let mut store = store_of(dir.path());
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 1,
                batch_id: "index-aborted",
                seq: 1,
                domain: "ingest",
                event_type: "ingest.started",
                source_key: None,
                outcome: None,
                reason_code: None,
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append the beginning of a batch that never ends");

        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(coverage.batch_id, "index-aborted", "the newest batch wins");
        assert!(!coverage.finished, "{coverage:?}");
        assert_eq!(coverage.events, 1);
        let line = format_coverage(&coverage);
        assert!(
            line.contains("no ingest.finished yet"),
            "the line says so: {line}"
        );
    }

    #[test]
    fn a_feed_row_does_not_hide_the_ingest_batch() {
        // ADR 0026 puts several domains in one table, so "the newest row" and
        // "the newest ingest batch" are different questions.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let mut store = store_of(dir.path());
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 1,
                batch_id: "feed-1",
                seq: 1,
                domain: "feed",
                event_type: "feed.poll.started",
                source_key: None,
                outcome: None,
                reason_code: None,
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append a feed row after the ingest batch");

        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage.batch_id, report.batch_id,
            "the ingest batch, not the newest row of any domain"
        );
        assert_eq!(coverage.indexed, 1);
    }

    #[test]
    fn reading_the_coverage_creates_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        write_config(dir.path(), &vault);

        assert!(
            coverage_report(dir.path()).expect("read").is_none(),
            "no store is no batch"
        );
        assert!(
            !dir.path().join(".momotaro").join("momotaro.db").exists(),
            "a read path must never be the reason a file appears"
        );
    }

    #[test]
    fn the_coverage_line_escapes_what_it_did_not_write() {
        // A `source_key` comes from a file name and POSIX allows control
        // characters in one: the line may not be able to move the cursor or
        // spoof a line, which is why the key is printed as a debug string.
        let report = CoverageReport {
            batch_id: "index-1".to_owned(),
            indexed: 0,
            skipped: 0,
            degradations: std::collections::BTreeMap::new(),
            warnings: std::collections::BTreeMap::new(),
            refreshed: 0,
            failures: vec![momotaro_contracts::CoverageFailure {
                source_key: Some("note:\u{1b}[31mred\u{1b}[0m".to_owned()),
                // The code is sanitized too, and this row is where that is
                // pinned: a foreign row can carry anything in this column.
                reason_code: Some("key_\u{1b}collision".to_owned()),
                message: Some("danger \u{202e}".to_owned()),
            }],
            events: 1,
            finished: false,
        };
        let line = format_coverage(&report);
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        assert!(line.contains("note:"), "the name stays readable: {line}");
        assert!(
            line.contains("key_collision"),
            "the code is readable once the escape is gone: {line:?}"
        );
    }

    #[test]
    fn an_uninitialized_store_is_not_an_empty_log() {
        // A 0-byte database is a workspace that was never written, and the answer
        // must not depend on whether some earlier command happened to touch it.
        let dir = tempfile::tempdir().expect("temp dir");
        let store_dir = dir.path().join(".momotaro");
        fs::create_dir_all(&store_dir).expect("create store dir");
        let database = store_dir.join("momotaro.db");
        fs::write(&database, b"").expect("create an empty database file");

        let error = coverage_report(dir.path()).expect_err("0 bytes is not a batch");
        assert!(
            matches!(&error, RunError::StoreUnhealthy(reason) if reason.contains("has not been initialized")),
            "{error}"
        );
        assert_eq!(
            fs::metadata(&database).expect("stat").len(),
            0,
            "a read path must never be the reason a file changes"
        );
    }

    #[test]
    fn a_store_this_build_cannot_use_is_named_as_such() {
        // Someone else's database (F7): the reader must say what `doctor` says,
        // not fall through to `no such table: store_events`.
        let dir = tempfile::tempdir().expect("temp dir");
        let store_dir = dir.path().join(".momotaro");
        fs::create_dir_all(&store_dir).expect("create store dir");
        let connection = rusqlite::Connection::open(store_dir.join("momotaro.db"))
            .expect("create a database that is not ours");
        connection
            .execute_batch("CREATE TABLE recipes (name TEXT); PRAGMA application_id = 1234567;")
            .expect("write a foreign database");
        drop(connection);

        let error = coverage_report(dir.path()).expect_err("a foreign store is not a log");
        assert!(
            matches!(&error, RunError::StoreUnhealthy(reason) if reason.contains("is not a Momotaro store")),
            "{error}"
        );
    }

    #[test]
    fn only_an_uninitialized_workspace_is_asked_to_run_init() {
        // The advice branch hangs off this predicate, so its exact meaning is
        // worth pinning: an *invalid* workspace is not one to send to `init`.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "# A\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);

        let fresh = doctor(dir.path()).expect("doctor on a fresh workspace");
        assert!(is_uninitialized(&fresh), "{fresh:?}");
        assert!(!is_healthy(&fresh), "{fresh:?}");

        init_workspace(dir.path()).expect("init");
        let ready = doctor(dir.path()).expect("doctor on an initialized workspace");
        assert!(!is_uninitialized(&ready), "{ready:?}");
        assert!(is_healthy(&ready), "{ready:?}");

        // A 0-byte database is `Uninitialized` by the store's own definition, so
        // `init` is the right advice for it too.
        let store_dir = dir.path().join(".momotaro");
        fs::write(store_dir.join("momotaro.db"), b"").expect("empty store");
        let empty = doctor(dir.path()).expect("doctor on an empty store");
        assert!(!is_healthy(&empty), "{empty:?}");
        assert!(is_uninitialized(&empty), "{empty:?}");

        // Someone else's database is not a missing one: `init` is not its advice.
        let connection =
            rusqlite::Connection::open(store_dir.join("momotaro.db")).expect("open the store file");
        connection
            .execute_batch("CREATE TABLE recipes (name TEXT); PRAGMA application_id = 1234567;")
            .expect("write a foreign database");
        drop(connection);
        let foreign = doctor(dir.path()).expect("doctor on a foreign workspace");
        assert!(!is_healthy(&foreign), "{foreign:?}");
        assert!(
            !is_uninitialized(&foreign),
            "a foreign store is not a missing store: {foreign:?}"
        );
    }

    #[test]
    fn frontmatter_stays_out_of_the_index_but_keeps_the_locator_honest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        // The block holds a *list*, which is the shape that can actually tell the
        // two implementations apart: a scalar-only block is a fixed point of the
        // "chunk the whole note" mutant (the parser folds it into a setext heading
        // that the body's own H1 then pops, and the offset cancels byte for byte),
        // so any assertion on it stays green either way.
        let note = "---\ntags:\n  - alphatoken\n---\n# Heading\n\nbody text here\n";
        fs::write(vault.join("a.md"), note).expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        // Asserted on our own artifact, not on a downstream hit: a hit depends on
        // how markdown classified the block's lines, which is exactly what makes
        // the assertion toothless.
        let store = store_of(dir.path());
        let chunks = store.all_current_chunks().expect("chunks");
        assert!(!chunks.is_empty(), "the note was chunked");
        for chunk in &chunks {
            assert!(
                !chunk.text.contains("alphatoken"),
                "block text reached a chunk's text: {:?}",
                chunk.text
            );
            assert!(
                !chunk
                    .heading_path
                    .clone()
                    .unwrap_or_default()
                    .contains("alphatoken"),
                "block text reached a heading path: {:?}",
                chunk.heading_path
            );
        }
        assert!(
            search_workspace(dir.path(), "alphatoken", 10)
                .expect("search")
                .hits
                .is_empty(),
            "and it is not searchable either"
        );

        // The body is indexed, and its locator counts lines from the note's first
        // line (the paragraph is line 7 of the note, line 3 of the body).
        let hits = search_workspace(dir.path(), "body text here", 10).expect("search");
        let hit = hits.hits.first().expect("the body is indexed");
        assert!(
            hit.hit.locator_json.contains("\"start_line\":7"),
            "locators are note-relative, not body-relative: {}",
            hit.hit.locator_json
        );

        // And the parsed fields are what lands in the blob — asserted structurally,
        // or deleting the field mapping would still pass on the raw block's text.
        let revision = store
            .current_revision("note:a.md")
            .expect("read")
            .expect("the note was indexed");
        let blob: serde_json::Value =
            serde_json::from_str(&revision.metadata_json).expect("the blob is JSON");
        assert_eq!(
            blob["frontmatter"]["state"], "parsed",
            "the block's state is in the blob: {blob}"
        );
        assert_eq!(blob["frontmatter"]["fields"][0]["key"], "tags", "{blob}");
        assert_eq!(
            blob["frontmatter"]["fields"][0]["value"]["list"][0], "alphatoken",
            "{blob}"
        );

        // The title comes from the body: a `#` inside the block is a YAML comment,
        // and letting it through would make the note's title (and the title the
        // search hit shows) a comment.
        fs::write(
            vault.join("b.md"),
            "---\n# Fakename\ntitle: x\n---\nbody text here too\n",
        )
        .expect("write the comment note");
        index_workspace(dir.path(), None).expect("reindex");
        let titled = store
            .current_revision("note:b.md")
            .expect("read")
            .expect("the second note was indexed");
        assert_eq!(
            titled.title.as_deref(),
            Some("b"),
            "the file stem, not the block's `#` comment"
        );
    }

    #[test]
    fn a_broken_block_costs_the_block_not_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(
            vault.join("a.md"),
            "---\ntitle: A\nnot a field\n---\nbody text here\n",
        )
        .expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        assert!(
            report.errors.is_empty(),
            "a bad block is not a bad file: {report:?}"
        );
        let hits = search_workspace(dir.path(), "body text here", 10).expect("search");
        assert_eq!(hits.hits.len(), 1, "the body was indexed anyway");

        let store = store_of(dir.path());
        let row = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:a.md"))
            .expect("a coverage row for the file");
        assert_eq!(row.outcome.as_deref(), Some("indexed"));
        assert_eq!(
            row.reason_code.as_deref(),
            Some("frontmatter_parse_error"),
            "the degradation is recorded beside the outcome, not instead of it"
        );
        assert_eq!(
            row.message.as_deref(),
            Some("the file was indexed; its frontmatter block could not be read (D41.5)"),
            "and the sentence is paired with the code where the code is produced"
        );
    }

    #[test]
    fn an_unreadable_file_reports_the_kind_it_had() {
        // The one production-reachable branch in this area that nothing else
        // drives: a file whose bytes are not valid UTF-8. Both halves are here on
        // purpose — the literal pins the spelling a reader would branch on, and
        // the joint invariant pins the row and the report to each other. A review
        // measured that neither half covers the other: without the literal, a
        // renamed kind stays green; without the invariant, the row may be hard
        // coded while the report is right.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("bad.md"), [0xff, 0xfe, 0x00, 0x41]).expect("write invalid utf-8");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        assert_eq!(
            report.errors.len(),
            1,
            "one bad file, one error: {report:?}"
        );
        let row = store_of(dir.path())
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:bad.md"))
            .expect("a coverage row for the file");
        assert_eq!(row.outcome.as_deref(), Some("error"));
        assert_eq!(
            row.reason_code.as_deref(),
            Some("invalid_utf8"),
            "the row carries the spelling a reader branches on"
        );
        assert_eq!(
            report.errors[0].kind,
            IndexFileErrorKind::InvalidUtf8,
            "the content was not valid UTF-8, and the kind says so (R-11)"
        );
        assert_eq!(
            row.reason_code.as_deref(),
            Some(report.errors[0].kind.to_string().as_str()),
            "and the row cannot disagree with the report that produced it"
        );
    }

    #[test]
    fn the_coverage_line_names_what_was_lost() {
        // `format_coverage` is pure, so the rendering can be checked without a
        // vault: a report that recorded a degradation must not read as a clean one,
        // and a warning (D42) must be rendered beside the degradations.
        let report = CoverageReport {
            batch_id: "index-1".to_owned(),
            indexed: 1,
            skipped: 1,
            refreshed: 0,
            degradations: [
                (
                    momotaro_contracts::Degradation::FrontmatterParseError,
                    momotaro_contracts::CoverageCounts {
                        in_index: 1,
                        skipped: 0,
                    },
                ),
                (
                    momotaro_contracts::Degradation::SymlinkEncountered,
                    momotaro_contracts::CoverageCounts {
                        in_index: 0,
                        skipped: 1,
                    },
                ),
                (
                    momotaro_contracts::Degradation::AttachmentUnindexed,
                    momotaro_contracts::CoverageCounts {
                        in_index: 1,
                        skipped: 2,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            warnings: [
                (
                    momotaro_contracts::Warning::WindowsIncompatibleName,
                    momotaro_contracts::CoverageCounts {
                        in_index: 2,
                        skipped: 1,
                    },
                ),
                (
                    momotaro_contracts::Warning::IcloudPlaceholder,
                    momotaro_contracts::CoverageCounts {
                        in_index: 0,
                        skipped: 1,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            failures: Vec::new(),
            events: 4,
            finished: true,
        };
        let line = format_coverage(&report);
        assert!(
            line.contains("1 x frontmatter_parse_error (1 indexed anyway)"),
            "the count is what stops a degraded file reading as clean: {line}"
        );
        // The row state follows the code, not one parenthetical for all of them: a
        // link is skipped, and printing "indexed anyway" beside it made the line
        // contradict its own skipped count.
        assert!(
            line.contains("1 x symlink_encountered (1 skipped: nothing was read)"),
            "{line}"
        );
        assert!(
            !line.contains("symlink_encountered (indexed anyway)"),
            "{line}"
        );
        // A code that rides both kinds of row — an attachment is in the index when it
        // was read and skipped when it was not — shows both, and the total is the sum.
        assert!(
            line.contains("3 x attachment_unindexed (1 indexed anyway, 2 skipped)"),
            "{line}"
        );
        // Warnings render through the same loop, from their own vocabulary: both
        // row states show, and the codes are the warning spellings.
        assert!(
            line.contains("3 x windows_incompatible_name (2 indexed anyway, 1 skipped)"),
            "{line}"
        );
        assert!(
            line.contains("1 x icloud_placeholder (1 skipped: nothing was read)"),
            "{line}"
        );
    }

    #[test]
    fn a_failed_block_records_a_stable_reason_code() {
        // §5.5 promises `metadata_json.frontmatter.reason` is a stable code rather
        // than a sentence. Reverting `reason.as_str()` to `reason.to_string()` used
        // to keep the whole suite green, because no test read a `failed` blob: the
        // only blob assertion in the repo looked at `state == "parsed"`.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(
            vault.join("prose.md"),
            "---\nThis is prose, not a field.\n---\nBody after.\n",
        )
        .expect("write the note");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        let revision = store_of(dir.path())
            .current_revision("note:prose.md")
            .expect("read")
            .expect("the note was indexed");
        let blob: serde_json::Value =
            serde_json::from_str(&revision.metadata_json).expect("the blob is JSON");
        let frontmatter = &blob["frontmatter"];
        assert_eq!(
            frontmatter["state"], "failed",
            "prose between two rules fails as a whole: {blob}"
        );
        let reason = frontmatter["reason"].as_str().expect("reason is a string");
        // One answer, one literal: prose between two rules is not a field, so
        // `not_a_field` is the code this note must carry. The three-code
        // alternation that used to stand here stayed green when two of the
        // spellings were swapped, which is not a witness for any of them.
        assert_eq!(
            reason, "not_a_field",
            "the code a reader branches on, spelled out: {blob}"
        );
        assert_eq!(
            frontmatter["line"], 2,
            "the source line is recorded: {blob}"
        );

        // The count a reader actually sees, read back out of the store. Deleting
        // `summarize_coverage`'s counting branch used to keep the whole suite
        // green, so the report could call a degraded file clean with nothing
        // noticing — the disease this whole series has been treating.
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::FrontmatterParseError)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "the batch knows the file lost its block: {coverage:?}"
        );
        assert_eq!(
            coverage.indexed, 1,
            "and it is still an indexed file, not a failure: {coverage:?}"
        );
    }

    fn store_of(root: &Path) -> Store {
        Store::open(database_path(root)).expect("open store")
    }

    fn current_hash_of(store: &Store, source_key: &str) -> String {
        store
            .current_revision(source_key)
            .expect("current revision read")
            .expect("current revision exists")
            .revision_hash
    }

    #[test]
    fn index_then_search_round_trips_hit_citations() {
        let (dir, _vault) = fixture_workspace();

        let report = index_workspace(dir.path(), None).expect("index");
        assert_eq!(report.files_scanned, 5);
        assert_eq!(report.revisions_created, 5);
        assert_eq!(report.files_indexed, 5);
        assert!(report.errors.is_empty());

        let output = search_workspace(dir.path(), "傅里叶", 10).expect("search");
        assert!(!output.hits.is_empty(), "傅里叶 query must hit fourier.md");
        let hit = &output.hits[0];
        assert_eq!(hit.hit.source_key, "note:fourier.md");
        assert!(hit.hit.rank >= 1);
        assert!(!hit.excerpt.is_empty());
        assert!(hit.title.is_some());

        let store = store_of(dir.path());
        let row = store
            .get_chunk(&hit.hit.chunk_id)
            .expect("chunk read")
            .expect("chunk exists");
        assert_eq!(row.source_key, hit.hit.source_key);
        assert_eq!(row.revision_hash, hit.hit.revision_hash);
        assert_eq!(row.chunk_hash, hit.hit.chunk_hash);
        assert_eq!(row.ordinal, hit.hit.ordinal);
    }

    #[test]
    fn reindex_unchanged_files_skips_and_never_duplicates() {
        let (dir, vault) = fixture_workspace();

        let first = index_workspace(dir.path(), None).expect("first index");
        assert_eq!(first.revisions_created, 5);
        let store = store_of(dir.path());
        let baseline = store.counts().expect("counts");

        // Same content, new mtime: must refresh metadata, not create revisions.
        let fourier = vault.join("fourier.md");
        let body = fs::read(&fourier).expect("read fourier");
        // Ensure the new mtime differs from the stored fingerprint at ms
        // granularity even on coarse filesystem clocks.
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&fourier, &body).expect("rewrite fourier");

        let second = index_workspace(dir.path(), None).expect("second index");
        assert_eq!(second.revisions_created, 0, "hash unchanged");
        assert_eq!(second.files_skipped, second.files_scanned - 1);
        assert_eq!(second.files_indexed, 1);

        let store = store_of(dir.path());
        let after = store.counts().expect("counts");
        assert_eq!(after.sources, baseline.sources);
        assert_eq!(after.revisions, baseline.revisions);
        assert_eq!(after.chunks, baseline.chunks);

        // Third run with no mutation at all: everything skipped.
        let third = index_workspace(dir.path(), None).expect("third index");
        assert_eq!(third.files_skipped, third.files_scanned);
        assert_eq!(third.files_indexed, 0);
    }

    #[test]
    fn content_edit_creates_exactly_one_new_revision_and_keeps_old() {
        let (dir, vault) = fixture_workspace();

        let first = index_workspace(dir.path(), None).expect("first index");
        assert_eq!(first.revisions_created, 5);
        let baseline = store_of(dir.path()).counts().expect("counts");

        let old_hash = {
            let store = store_of(dir.path());
            current_hash_of(&store, "note:fourier.md")
        };
        // Deterministic id of the old revision's first chunk (ordinal 0
        // always exists; fourier.md chunks several paragraphs).
        let old_chunk_id = momotaro_ingest::chunk_id("note:fourier.md", &old_hash, 0);

        let fourier = vault.join("fourier.md");
        let mut body = fs::read_to_string(&fourier).expect("read fourier");
        body.push_str("\n\n新增段落：傅里叶级数把周期函数展开为正弦与余弦的叠加。\n");
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&fourier, body).expect("append paragraph");

        let second = index_workspace(dir.path(), None).expect("second index");
        assert_eq!(second.revisions_created, 1, "only the edited file");
        assert_eq!(second.files_skipped, second.files_scanned - 1);

        let store = store_of(dir.path());
        let new_hash = current_hash_of(&store, "note:fourier.md");
        assert_ne!(new_hash, old_hash, "new content must produce new hash");

        let after = store.counts().expect("counts");
        assert_eq!(after.revisions, baseline.revisions + 1, "one new revision");
        assert_eq!(after.sources, baseline.sources, "no new source");

        // The old revision row is retained with is_current demoted (the
        // current revision is now `new_hash`), and its chunk rows still
        // resolve under the old revision hash.
        assert_ne!(new_hash, old_hash);
        let old_chunk = store
            .get_chunk(&old_chunk_id)
            .expect("old chunk read")
            .expect("old chunk row retained after re-revision");
        assert_eq!(old_chunk.revision_hash, old_hash);
        assert_eq!(old_chunk.source_key, "note:fourier.md");
    }

    #[test]
    fn search_after_index_deletion_fails_then_rebuild_restores() {
        let (dir, _vault) = fixture_workspace();

        let before = index_workspace(dir.path(), None).expect("index");
        let pre_search = search_workspace(dir.path(), "傅里叶", 10).expect("search");
        assert!(!pre_search.hits.is_empty());

        let store = store_of(dir.path());
        let counts_before = store.counts().expect("counts");
        drop(store);

        discard(&index_dir(dir.path())).expect("lose the index");

        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("no index");
        // The store still holds the chunks, so this is not "never indexed": the
        // only command that can put them back is the rebuild.
        assert!(
            matches!(&error, RunError::IndexUnusable(reason) if reason.contains("rebuild-index")),
            "{error:?}"
        );

        let rebuild = rebuild_index(dir.path()).expect("rebuild");
        assert_eq!(rebuild.chunks, before.chunks_written);
        assert!(
            format_rebuild(&rebuild).contains("discarded an unusable index"),
            "and the human line says so: {}",
            format_rebuild(&rebuild)
        );

        let store = store_of(dir.path());
        let counts_after = store.counts().expect("counts");
        assert_eq!(
            format!("{counts_before:?}"),
            format!("{counts_after:?}"),
            "store rows untouched by rebuild"
        );
        assert_eq!(counts_after, counts_before);

        let output = search_workspace(dir.path(), "傅里叶", 10).expect("search");
        let hits_now: Vec<&momotaro_contracts::RetrievalHit> = output
            .hits
            .iter()
            .map(|search_hit| &search_hit.hit)
            .collect();
        let hits_before: Vec<&momotaro_contracts::RetrievalHit> = pre_search
            .hits
            .iter()
            .map(|search_hit| &search_hit.hit)
            .collect();
        assert_eq!(hits_now, hits_before, "rebuild restores identical hits");
    }

    #[test]
    fn a_stale_index_is_replaced_by_rebuild_and_flagged_by_doctor() {
        let (dir, _vault) = fixture_workspace();
        let before = index_workspace(dir.path(), None).expect("index");
        assert!(before.chunks_written > 0);

        // An index written before `origin_class` existed: drop that field from the
        // on-disk schema, which is exactly what a build from before the change
        // leaves behind in a workspace it has already indexed.
        let meta_path = index_dir(dir.path()).join("meta.json");
        let mut meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&meta_path).expect("meta.json exists"))
                .expect("meta.json is JSON");
        let schema = meta["schema"].as_array_mut().expect("schema array");
        let fields = schema.len();
        schema.retain(|entry| entry["name"] != "origin_class");
        assert_eq!(schema.len(), fields - 1, "the field was there to drop");
        fs::write(&meta_path, meta.to_string()).expect("write the stale meta");

        // Retrieval is broken, and it says why.
        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("stale index");
        assert!(
            format!("{error}").contains("origin_class"),
            "the mismatch names the field: {error}"
        );

        // The store is fine, so doctor may not call the workspace ready: a healthy
        // report for a workspace that cannot search is a lie.
        let report = doctor(dir.path()).expect("doctor");
        assert_eq!(report.status, WorkspaceStatus::Invalid);
        let message = format_health(&report);
        assert!(
            message.contains("origin_class"),
            "names the field: {message}"
        );
        assert!(message.contains("rebuild"), "names the remedy: {message}");

        // The remedy the message names must be executable: `rebuild-index` cannot
        // dead-end on the very mismatch it exists to fix.
        let rebuilt = rebuild_index(dir.path()).expect("rebuild replaces a stale index");
        assert_eq!(rebuilt.chunks, before.chunks_written);
        let reason = rebuilt.replaced_stale.as_deref().expect("says why");
        assert!(reason.contains("origin_class"), "the reason: {reason}");
        assert!(
            format_rebuild(&rebuilt).contains("discarded an unusable index"),
            "a discarded artifact is said out loud: {}",
            format_rebuild(&rebuilt)
        );

        // And the workspace works again.
        assert_eq!(
            doctor(dir.path()).expect("doctor").status,
            WorkspaceStatus::Ready
        );
        let output = search_workspace(dir.path(), "傅里叶", 10).expect("search");
        assert!(!output.hits.is_empty(), "retrieval is back");
    }

    #[test]
    fn rebuild_replaces_an_index_it_cannot_even_open() {
        // The other half of the same promise: a *damaged* derived artifact (an
        // interrupted write, a full disk) is not something a user can fix by
        // hand, and the index is rebuildable from canonical storage either way.
        let (dir, _vault) = fixture_workspace();
        let before = index_workspace(dir.path(), None).expect("index");
        let meta_path = index_dir(dir.path()).join("meta.json");
        fs::write(&meta_path, b"{\"segments\":[").expect("truncate the meta");

        let error = search_workspace(dir.path(), "傅里叶", 10)
            .expect_err("a damaged index is an error, never an empty result set");
        assert!(
            format!("{error}").contains("meta.json"),
            "the reason comes from the index itself: {error}"
        );

        let rebuilt = rebuild_index(dir.path()).expect("rebuild must replace a damaged index");
        assert_eq!(rebuilt.chunks, before.chunks_written);
        let reason = rebuilt.replaced_stale.as_deref().expect("says why");
        assert!(reason.contains("meta.json"), "{reason}");
        assert!(
            !search_workspace(dir.path(), "傅里叶", 10)
                .expect("search")
                .hits
                .is_empty(),
            "retrieval is back"
        );
    }

    #[test]
    fn never_indexed_says_index_and_a_lost_index_says_rebuild() {
        let (dir, _vault) = fixture_workspace();
        init_workspace(dir.path()).expect("init");

        // Never indexed: nothing is wrong, there is simply nothing in the index
        // yet, so the advice is the command that fills it.
        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("no index");
        assert!(matches!(error, RunError::IndexNotBuilt(_)), "{error:?}");
        assert_eq!(
            doctor(dir.path()).expect("doctor").status,
            WorkspaceStatus::Ready
        );

        index_workspace(dir.path(), None).expect("index");
        discard(&index_dir(dir.path())).expect("lose the index");

        // Lost after having one: the store still holds the chunks, so `index`
        // cannot put them back and every command names the one that can.
        let error = index_workspace(dir.path(), None).expect_err("index cannot put it back");
        assert!(matches!(error, RunError::IndexUnusable(_)), "{error:?}");
        assert_eq!(
            doctor(dir.path()).expect("doctor").status,
            WorkspaceStatus::Invalid
        );
    }

    #[test]
    fn a_read_only_command_never_creates_the_database() {
        // The fault predicate opens the store, and `Store::open` creates the file
        // it is handed: it must look first. A query in a workspace whose database
        // is gone leaves that workspace exactly as it found it.
        let (dir, _vault) = fixture_workspace();
        init_workspace(dir.path()).expect("init");
        let database = dir.path().join(".momotaro").join("momotaro.db");
        fs::remove_file(&database).expect("clean the database away");

        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("no store");
        assert!(matches!(error, RunError::IndexNotBuilt(_)), "{error:?}");
        assert!(
            !database.exists(),
            "a query must not create the database it read"
        );
    }

    #[test]
    fn a_store_that_is_not_ready_is_reported_before_the_missing_index() {
        let (dir, _vault) = fixture_workspace();
        fs::create_dir_all(dir.path().join(".momotaro")).expect("data dir");
        fs::write(dir.path().join(".momotaro").join("momotaro.db"), b"")
            .expect("an uninitialized database");

        // Nothing can be indexed yet, so "run `index`" would be advice that fails
        // the same way; the store's own report is the useful one.
        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("not ready");
        assert!(
            matches!(&error, RunError::StoreUnhealthy(reason) if reason.contains("not been initialized")),
            "{error:?}"
        );
        // Saying "uninitialized" must not be the thing that initialises it: an
        // empty file is classified without opening it.
        assert_eq!(
            fs::metadata(dir.path().join(".momotaro").join("momotaro.db"))
                .expect("database metadata")
                .len(),
            0,
            "a read path must not write the database it looked at"
        );
    }

    #[test]
    fn a_query_never_creates_the_database_it_has_to_read() {
        // The index is there, but the store that gives the hits their titles and
        // excerpts is gone: the query has to say so rather than answer with
        // empty citations — and it certainly may not create the database on its
        // way past.
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");
        let database = dir.path().join(".momotaro").join("momotaro.db");
        fs::remove_file(&database).expect("remove the database");

        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("no store to cite");
        assert!(
            matches!(&error, RunError::StoreUnhealthy(reason) if reason.contains("missing or empty")),
            "{error:?}"
        );
        assert!(!database.exists(), "the query must not create the database");
    }

    /// Every file under `dir` — relative path and bytes — so "untouched" can be
    /// asserted as bytes rather than as "the command still worked".
    fn dir_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in fs::read_dir(&current).expect("read dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let name = path
                        .strip_prefix(dir)
                        .expect("under dir")
                        .display()
                        .to_string();
                    entries.push((name, fs::read(&path).expect("read file")));
                }
            }
        }
        entries.sort();
        entries
    }

    #[test]
    fn a_failed_rebuild_leaves_the_previous_index_in_place() {
        let (dir, _vault) = fixture_workspace();
        let before = index_workspace(dir.path(), None).expect("index");

        // Block the path the replacement is built on: the rebuild cannot produce
        // anything, and the index that is serving searches right now must not be
        // touched — earlier this command deleted the old index before building
        // the new one.
        let staging = staging_dir(dir.path());
        fs::write(&staging, b"blocked").expect("block the staging path");
        let live = dir_bytes(&index_dir(dir.path()));

        let error = rebuild_index(dir.path()).expect_err("staging is blocked");
        assert!(
            matches!(&error, RunError::StagingBlocked { path } if path == &staging),
            "names what is in the way: {error:?}"
        );
        assert!(
            format!("{error}").contains("in the way"),
            "and says what to do: {error}"
        );
        assert_eq!(
            dir_bytes(&index_dir(dir.path())),
            live,
            "the live index is untouched, byte for byte"
        );
        assert!(
            !search_workspace(dir.path(), "傅里叶", 10)
                .expect("the live index still answers")
                .hits
                .is_empty()
        );
        assert_eq!(
            doctor(dir.path()).expect("doctor").status,
            WorkspaceStatus::Ready
        );

        fs::remove_file(&staging).expect("unblock");
        let rebuilt = rebuild_index(dir.path()).expect("rebuild");
        assert_eq!(rebuilt.chunks, before.chunks_written);
        assert!(rebuilt.replaced_stale.is_none(), "{rebuilt:?}");
    }

    #[test]
    fn rebuild_is_idempotent_and_counts_match_current_chunks() {
        let (dir, _vault) = fixture_workspace();

        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let counts = store.counts().expect("counts");
        drop(store);

        let first = rebuild_index(dir.path()).expect("rebuild once");
        let second = rebuild_index(dir.path()).expect("rebuild twice");
        assert!(
            first.replaced_stale.is_none() && second.replaced_stale.is_none(),
            "a healthy index is not reported as replaced: {first:?}"
        );
        assert_eq!(first.chunks, second.chunks, "rebuild is idempotent");
        // Ingest's own write counter, not a second read of the list the rebuild
        // replays: two code paths that must agree on how many chunks exist.
        assert_eq!(
            first.chunks, report.chunks_written,
            "rebuild covers exactly what ingest wrote"
        );

        let store = store_of(dir.path());
        let after = store.counts().expect("counts");
        assert_eq!(after, counts, "store rows untouched by rebuilds");
    }

    #[test]
    fn search_output_serializes_flattened() {
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        let output = search_workspace(dir.path(), "eigenvalue", 5).expect("search");
        if let Some(hit) = output.hits.first() {
            let encoded = serde_json::to_value(hit).expect("encode hit");
            assert!(encoded.get("source_key").is_some(), "flattened field");
            assert!(encoded.get("chunk_id").is_some(), "flattened field");
            assert!(encoded.get("excerpt").is_some(), "own field");
            assert!(encoded.get("title").is_some(), "own field");
            assert!(encoded.get("hit").is_none(), "hit must not nest");
        }
    }

    #[test]
    fn search_empty_and_punct_queries_return_empty_output() {
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        let empty = search_workspace(dir.path(), "", 5).expect("empty query");
        assert!(empty.hits.is_empty());
        let punct = search_workspace(dir.path(), "!!!", 5).expect("punct query");
        assert!(punct.hits.is_empty());
    }

    #[test]
    fn stale_fingerprint_falls_through_to_hash_refresh() {
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        // Wipe the fingerprint with a non-matching JSON blob *and* the stored
        // path with a stale one (what a directory move leaves behind), so the
        // fast path must fall through to the hash check and repair the row
        // instead of skipping forever.
        {
            let mut store = store_of(dir.path());
            let hash = current_hash_of(&store, "note:fourier.md");
            store
                .update_revision_fingerprint(
                    "note:fourier.md",
                    &hash,
                    "{\"mtime_ms\":0}",
                    "moved/away.md",
                    "away.md",
                )
                .expect("clobber the mutable columns");
        }

        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(report.revisions_created, 0, "hash unchanged");
        assert_eq!(report.files_indexed, 1, "the row is refreshed in place");
        assert_eq!(report.files_skipped, 4, "the untouched four are skipped");

        let store = store_of(dir.path());
        let revision = store
            .current_revision("note:fourier.md")
            .expect("read")
            .expect("exists");
        assert!(
            revision
                .metadata_json
                .contains("\"tool\":\"momotaro-index-v1\""),
            "fingerprint restored: {}",
            revision.metadata_json
        );
        assert_ne!(revision.metadata_json, "{\"mtime_ms\":0}");
        assert_eq!(
            revision.local_path.as_deref(),
            Some("vault/fourier.md"),
            "a stale path is repaired, not left behind"
        );
        assert_eq!(revision.raw_name.as_deref(), Some("fourier.md"));
    }

    #[test]
    fn index_workspace_with_path_override_uses_given_vault() {
        let (dir, _vault) = fixture_workspace();
        // The override must live inside the workspace: since ADR 0024 every
        // stored path is workspace-relative (an escaping override is refused —
        // see `an_override_outside_the_workspace_is_refused`).
        let alt = dir.path().join("alt-vault");
        fs::create_dir_all(&alt).expect("create alt vault");
        fs::write(
            alt.join("only.md"),
            "# Override\n\n路径覆盖傅里叶验证段落。\n",
        )
        .expect("write alt note");

        let report = index_workspace(dir.path(), Some(&alt)).expect("override index");
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.revisions_created, 1);

        let output = search_workspace(dir.path(), "傅里叶", 10).expect("search");
        assert!(
            output
                .hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:only.md"),
            "override vault content must be searchable"
        );
    }

    #[test]
    fn an_override_outside_the_workspace_is_refused() {
        let (dir, _vault) = fixture_workspace();
        let outside = tempfile::tempdir().expect("outside vault");
        fs::write(outside.path().join("only.md"), "# Outside\n\nbody\n").expect("write note");

        let error = index_workspace(dir.path(), Some(outside.path()))
            .expect_err("an override outside the workspace must be refused");
        assert!(
            matches!(error, RunError::VaultOutsideWorkspace { .. }),
            "{error}"
        );
    }

    #[test]
    fn doctor_reports_a_vault_outside_the_workspace_as_invalid() {
        // Doctor stays non-mutating and never fails on a misconfigured
        // workspace: it reports what is wrong, and the CLI must not print that
        // as "workspace is ready" (see `format_health`).
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("ws");
        let outside = dir.path().join("outside-vault");
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&outside).expect("create outside vault");
        write_config(&root, &outside);

        let report = doctor(&root).expect("doctor reports instead of failing");
        assert_eq!(report.status, WorkspaceStatus::Invalid, "{report:?}");
        assert!(
            report
                .message
                .as_deref()
                .is_some_and(|message| message.contains("outside workspace")),
            "{report:?}"
        );
        assert!(
            !format_health(&report).contains("is ready"),
            "an invalid workspace is never rendered as ready"
        );
    }

    #[test]
    fn an_unhealthy_report_is_not_rendered_as_ready() {
        let report = HealthReport {
            status: WorkspaceStatus::Invalid,
            schema_version: None,
            sqlite: None,
            message: Some("vault /x is outside workspace /y".to_owned()),
            network_volume: None,
        };
        let line = format_health(&report);
        assert!(line.contains("Invalid"), "{line}");
        assert!(line.contains("outside workspace"), "{line}");
        assert!(!line.contains("is ready"), "{line}");

        let ready = HealthReport {
            status: WorkspaceStatus::Ready,
            schema_version: Some(momotaro_contracts::SchemaVersion(3)),
            sqlite: None,
            message: None,
            network_volume: None,
        };
        assert_eq!(format_health(&ready), "workspace is ready");
    }

    #[test]
    fn doctor_reports_the_engine_that_writes_the_database() {
        let (dir, _vault) = fixture_workspace();
        init_workspace(dir.path()).expect("init");

        let report = doctor(dir.path()).expect("doctor");
        let sqlite = report.sqlite.expect("engine facts are always reported");
        assert_eq!(sqlite.version_number, 3_053_002);
        assert_eq!(sqlite.version, "3.53.2");
        assert!(
            !sqlite.compile_options.is_empty(),
            "compile options are reported"
        );
        assert!(
            !sqlite.wal_reset_window,
            "this build ships an engine out of the window"
        );
        assert!(
            !index_dir(dir.path()).exists(),
            "doctor is non-mutating: it must not create the derived index"
        );
    }

    #[test]
    fn doctor_reports_engine_facts_even_before_init() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("ws");
        fs::create_dir_all(root.join("vault")).expect("create vault");
        write_config(&root, &root.join("vault"));

        let report = doctor(&root).expect("doctor");
        assert_eq!(report.status, WorkspaceStatus::Uninitialized, "{report:?}");
        assert!(
            report.sqlite.is_some(),
            "the engine version matters most when the database is missing"
        );
        assert!(!is_healthy(&report));
    }

    #[test]
    fn an_engine_inside_the_wal_reset_window_is_reported_as_invalid() {
        // The window corrupts WAL databases, so `doctor` must not merely warn.
        // Facts are injected: this build links a fixed engine, and an
        // untestable hard failure is a hard failure nobody checks.
        let (dir, _vault) = fixture_workspace();
        init_workspace(dir.path()).expect("init");

        let in_window = SqliteFacts {
            version: "3.50.2".to_owned(),
            version_number: 3_050_002,
            source_id: "2025-01-01 00:00:00 test".to_owned(),
            compile_options: Vec::new(),
            wal_reset_window: true,
            withdrawn_release: false,
        };

        let report = doctor_with_facts(dir.path(), &in_window).expect("doctor");
        assert_eq!(
            report.status,
            WorkspaceStatus::Invalid,
            "the window is a refusal, not a warning"
        );
        assert!(!is_healthy(&report));
        let message = report.message.expect("a refusal says why");
        assert!(message.contains("3.50.2"), "{message}");
        assert!(
            message.contains("3.51.3"),
            "the fixed release is named: {message}"
        );
        assert!(
            !message.contains("3.52"),
            "the withdrawn release is never advised: {message}"
        );
        assert_eq!(
            report.sqlite.map(|facts| facts.version),
            Some("3.50.2".to_owned()),
            "and the offending engine is named in the report"
        );

        // The same workspace with the engine this build links is ready.
        let fixed = momotaro_store::sqlite_facts().expect("facts");
        let healthy = doctor_with_facts(dir.path(), &fixed).expect("doctor");
        assert_eq!(healthy.status, WorkspaceStatus::Ready, "{healthy:?}");
    }

    #[test]
    fn a_withdrawn_release_is_refused_with_its_own_reason() {
        let facts = SqliteFacts {
            version: "3.52.0".to_owned(),
            version_number: 3_052_000,
            source_id: "2026-01-01 00:00:00 test".to_owned(),
            compile_options: Vec::new(),
            wal_reset_window: false,
            withdrawn_release: true,
        };

        let message = engine_refusal_message(&facts).expect("a withdrawn release is refused");
        assert!(message.contains("withdrawn"), "{message}");
        assert!(message.contains("3.52.0"), "{message}");
        assert!(
            message.contains("3.51.3"),
            "the way out is named: {message}"
        );

        let fixed = SqliteFacts {
            withdrawn_release: false,
            ..facts
        };
        assert!(engine_refusal_message(&fixed).is_none());
    }

    /// D42: two distinct files must never fold into one identity. NFC "café.md"
    /// and NFD "cafe\u{0301}.md" are two files that share one key; until the
    /// store event table can carry the collision policy, the second claimant
    /// must be refused, never silently overwrite the first.
    #[test]
    fn colliding_keys_are_reported_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).unwrap();
        fs::write(vault.join("caf\u{00e9}.md"), "# A\n\nnfc marker\n").unwrap();
        fs::write(vault.join("cafe\u{0301}.md"), "# B\n\nnfd marker\n").unwrap();
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");

        let report = index_workspace(dir.path(), None).expect("index");
        assert_eq!(report.files_scanned, 2);
        assert_eq!(report.files_indexed, 1, "first writer wins");
        assert_eq!(report.errors.len(), 1, "the collision is reported");

        assert_eq!(
            report.errors[0].kind,
            IndexFileErrorKind::KeyCollision,
            "callers branch on the typed kind, not on message text"
        );
        let message = &report.errors[0].message;
        // Both spellings must be distinguishable in the text ...
        assert!(
            message.contains(r#"cafe\u{301}.md"#),
            "escaped NFD: {message}"
        );
        assert!(
            message.contains("caf\u{00e9}.md"),
            "NFC spelling: {message}"
        );

        let store = store_of(dir.path());
        assert_eq!(store.counts().expect("counts").sources, 1);
        // ... and exactly one claimant's bytes may have reached the store.
        let chunks = store.all_current_chunks().expect("chunks");
        let nfc = chunks.iter().any(|chunk| chunk.text.contains("nfc marker"));
        let nfd = chunks.iter().any(|chunk| chunk.text.contains("nfd marker"));
        assert!(nfc ^ nfd, "exactly one claimant's content must be stored");

        // A re-run reports the same collision instead of quietly accepting it:
        // 2 files = 0 indexed (the survivor's fingerprint is unchanged) +
        // 1 skipped + 1 refused.
        let again = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(again.errors.len(), 1, "collision reported on every run");
        assert_eq!(again.files_indexed, 0, "the survivor is unchanged");
        assert_eq!(
            again.files_skipped, 1,
            "the survivor is skipped, not re-ingested"
        );
    }

    #[test]
    fn index_titles_and_heading_paths_flow_to_search() {
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let title = store
            .current_revision_title("note:fourier.md")
            .expect("title read")
            .expect("derived title");
        assert_eq!(title, "傅里叶变换");

        let output = search_workspace(dir.path(), "卷积", 10).expect("search");
        assert!(!output.hits.is_empty(), "zh body query must hit");
        let with_heading = output
            .hits
            .iter()
            .find(|hit| hit.hit.heading_path.is_some())
            .expect("a chunk with a heading is indexed");
        let stored = store
            .get_chunk(&with_heading.hit.chunk_id)
            .expect("chunk read")
            .expect("the hit's chunk is in the store");
        assert_eq!(
            with_heading.hit.heading_path, stored.heading_path,
            "the hit's heading is the canonical chunk's heading"
        );
        assert_eq!(
            with_heading.hit.heading_path.as_deref(),
            Some("傅里叶变换 > 基本定义"),
            "…and the chain is the one the fixture pins, so a chunker change cannot move both sides together"
        );
    }

    #[test]
    fn fixture_files_each_produce_a_revision() {
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        // The `empty.md` fixture is heading + one body paragraph, so it
        // yields exactly one chunk carrying the heading path. A truly
        // heading-only or whitespace file would yield none (covered by the
        // chunker's own unit tests).
        let store = store_of(dir.path());
        let chunks = store.all_current_chunks().expect("current chunks");
        let empty_chunks: Vec<_> = chunks
            .iter()
            .filter(|chunk| chunk.source_key == "note:empty.md")
            .collect();
        assert_eq!(empty_chunks.len(), 1);
        assert_eq!(empty_chunks[0].heading_path.as_deref(), Some("Empty Note"));
        assert_eq!(
            store.counts().expect("counts").sources,
            5,
            "every scanned file records a revision"
        );
    }

    // ------------------------------------------------------------------
    // Perf smoke (ignored by default): 1000 files through init+index+search.
    // ------------------------------------------------------------------

    #[test]
    #[ignore]
    fn perf_smoke_thousand_files_index_and_search() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        write_config(dir.path(), &vault);

        for i in 0..1000 {
            // The ASCII space before the marker matters: the cjk bigram
            // tokenizer folds CJK punctuation into runs, so "。第517号…"
            // would bigram "。第" and a phrase query for "第517号标记"
            // could not align. A space keeps the marker's token sequence
            // contiguous.
            fs::write(
                vault.join(format!("note-{i}.md")),
                format!(
                    "# 笔记第{i}号\n\n这是一篇普通的学习笔记。唯一标记 第{i}号标记 用于检索验证。\n\n正文继续讨论线性代数中的矩阵分解与特征值问题。\n"
                ),
            )
            .expect("write note");
        }

        let started = std::time::Instant::now();
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");
        assert_eq!(report.revisions_created, 1000);
        assert_eq!(report.files_scanned, 1000);
        let output = search_workspace(dir.path(), "第517号标记", 10).expect("search");
        assert!(
            output
                .hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:note-517.md"),
            "sampled marker must be findable"
        );
        println!(
            "perf smoke: index+search of 1000 files took {} ms (index report {} ms)",
            started.elapsed().as_millis(),
            report.duration_ms
        );
    }

    #[test]
    fn a_canvas_contributes_its_text_and_not_its_ids() {
        // §5 第 9 条: only the fields the spec gives a text meaning reach the index. A
        // node's `id` is a random hex string that is unique only inside its own
        // canvas, so indexing it would add tokens no query can mean.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        let canvas = r##"{
  "nodes": [
    {"id": "8f1a2b3c4d5e6f70", "type": "text", "text": "alphatoken card", "x": 10, "y": 20, "width": 100, "height": 50, "color": "4"},
    {"id": "1122334455667788", "type": "file", "file": "readme.md", "subpath": "#Headingtoken", "x": 0, "y": 0},
    {"id": "99aabbccddeeff00", "type": "link", "url": "https://example.com/linktoken"},
    {"id": "0011223344556677", "type": "group", "label": "grouptoken", "x": 0, "y": 0},
    {"id": "deadbeefdeadbeef", "type": "group"},
    {"id": "cafebabecafebabe", "type": "no-such-kind", "text": "shouldnotappear"}
  ],
  "edges": [
    {"id": "e1", "fromNode": "8f1a2b3c4d5e6f70", "toNode": "1122334455667788", "label": "edgelabel"},
    {"id": "e2", "fromNode": "8f1a2b3c4d5e6f70", "toNode": "not-an-id"}
  ]
}"##;
        fs::write(vault.join("board.canvas"), canvas).expect("write canvas");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");
        assert_eq!(
            report.revisions_created, 1,
            "the canvas is a source: {report:?}"
        );

        // What the file is about is searchable, and points at the canvas.
        for query in ["alphatoken", "linktoken", "grouptoken"] {
            let hits = search_workspace(dir.path(), query, 10).expect("search");
            assert!(
                hits.hits
                    .iter()
                    .any(|hit| hit.hit.source_key == "note:board.canvas"),
                "{query} must be findable: {hits:?}"
            );
        }
        // What the file is *not* about is not: ids, geometry and colors stay out.
        for absent in ["cafebabecafebabe", "8f1a2b3c4d5e6f70", "shouldnotappear"] {
            let hits = search_workspace(dir.path(), absent, 10).expect("search");
            assert!(hits.hits.is_empty(), "{absent} reached the index: {hits:?}");
        }

        // The text the node fields spell out, read back out of the store.
        let chunks = store_of(dir.path()).all_current_chunks().expect("chunks");
        let text: String = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(text.contains("readme.md#Headingtoken"), "{text:?}");
        assert!(
            text.contains("alphatoken card -> readme.md#Headingtoken: edgelabel"),
            "the edge names its ends by their text, with the label: {text:?}"
        );
        assert!(
            text.contains("alphatoken card -> not-an-id"),
            "a dangling end keeps the id as written: {text:?}"
        );
        // Order is the spec's own (array order is the z-index), and the edges come
        // after the nodes. `contains` alone cannot see a reordering — the token set
        // is the same — so the positions are compared instead. Whether a node with
        // no text produced a segment at all is pinned where the text is built
        // (`canvas_text`'s own test), because that is the only level where a
        // placeholder can be seen for what it is.
        let order: Vec<usize> = [
            "alphatoken card",
            "readme.md#Headingtoken",
            "https://example.com/linktoken",
            "grouptoken",
            "alphatoken card -> readme.md#Headingtoken: edgelabel",
            "alphatoken card -> not-an-id",
        ]
        .iter()
        .map(|needle| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing: {text:?}"))
        })
        .collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "the segments follow the arrays' order: {order:?} in {text:?}"
        );

        // A canvas's chunks point at the file: no byte range inside it spells the
        // rendered text out, and a citation still has to resolve to the document.
        // The axes are the *normalised* text's (BOM off, CRLF to LF) — the rule
        // every locator in this crate follows — so the expectation is the
        // normalised length rather than the file's byte count.
        let canvas_bytes = fs::read(vault.join("board.canvas")).expect("read canvas");
        let canvas_normalized =
            momotaro_ingest::normalize_markdown(&canvas_bytes).expect("the canvas normalises");
        for chunk in &chunks {
            let locator: serde_json::Value =
                serde_json::from_str(&chunk.locator_json).expect("locator is valid JSON");
            assert_eq!(locator["start_byte"].as_u64(), Some(0), "{locator}");
            assert_eq!(locator["start_line"].as_u64(), Some(1), "{locator}");
            assert_eq!(
                locator["end_byte"].as_u64(),
                Some(canvas_normalized.len() as u64),
                "the window is the file, not the rendered text: {locator}"
            );
        }

        // A CRLF canvas is where the two numbers part company: the window follows
        // the normalised text, so it is shorter than the file by one byte per line —
        // a consumer that slices the file must normalise first.
        let crlf = canvas.replace('\n', "\r\n");
        fs::write(vault.join("crlf.canvas"), &crlf).expect("write the CRLF canvas");
        index_workspace(dir.path(), None).expect("index the CRLF canvas");
        let crlf_bytes = fs::read(vault.join("crlf.canvas")).expect("read the CRLF canvas");
        assert!(
            crlf_bytes.len() > canvas_normalized.len(),
            "the fixture really is CRLF: {}",
            crlf_bytes.len()
        );
        let crlf_chunk = store_of(dir.path())
            .all_current_chunks()
            .expect("chunks")
            .into_iter()
            .find(|chunk| chunk.source_key == "note:crlf.canvas")
            .expect("the CRLF canvas is indexed");
        let locator: serde_json::Value =
            serde_json::from_str(&crlf_chunk.locator_json).expect("locator is valid JSON");
        let crlf_normalized =
            momotaro_ingest::normalize_markdown(&crlf_bytes).expect("the canvas normalises");
        assert_eq!(
            locator["end_byte"].as_u64(),
            Some(crlf_normalized.len() as u64),
            "the window is the normalised text, not the file's bytes: {locator}"
        );
    }

    #[test]
    fn an_empty_canvas_is_indexed_without_a_code() {
        // Both arrays are optional in the spec, so a canvas with nothing in it is
        // zero segments — not a failure, and not a placeholder line either.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("empty.canvas"), "{}").expect("write canvas");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        assert!(
            store_of(dir.path())
                .current_revision("note:empty.canvas")
                .expect("read")
                .is_some(),
            "the file is a source even when it says nothing"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert!(
            coverage.degradations.is_empty(),
            "an empty canvas lost nothing: {coverage:?}"
        );
        assert_eq!(coverage.indexed, 1, "{coverage:?}");
    }

    #[test]
    fn a_broken_canvas_keeps_its_own_text_and_reports_the_code() {
        // The spec says nothing about unreadable JSON (no schema, no error chapter),
        // so our own rule applies: keep the content, report the code, never drop the
        // file (plan trap ②, §5 第 9 条).
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        let broken =
            "{\"nodes\": [{\"id\": \"a\", \"type\": \"text\", \"text\": \"brokenmarker\"},]}";
        fs::write(vault.join("broken.canvas"), broken).expect("write canvas");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        // Kept: the file's own text is what gets indexed, and it is findable.
        let hits = search_workspace(dir.path(), "brokenmarker", 10).expect("search");
        assert!(
            hits.hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:broken.canvas"),
            "the content is not lost: {hits:?}"
        );
        // Reported: the row still says the file was indexed, and carries the code.
        let row = store_of(dir.path())
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:broken.canvas"))
            .expect("a coverage row for the canvas");
        assert_eq!(row.outcome.as_deref(), Some("indexed"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("canvas_parse_error"),
            "the spelling a reader branches on: {row:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::CanvasParseError)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "{coverage:?}"
        );
    }

    #[test]
    fn an_excalidraw_scene_stays_out_of_the_index() {
        // The plugin stores the drawing as a fenced block; `%%` only hides it when
        // the note is rendered, so every byte of the compressed scene used to reach
        // the full-text index while the note's real text sat next to it (§5 第 9 条).
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        let note = "---\nexcalidraw-plugin: parsed\ntags: [excalidraw]\n---\n\n# Excalidraw Data\n\n## Text Elements\ncardtoken\n\n%%\n## Drawing\n```compressed-json\nSCENENOISEtoken\n```\n%%\n";
        fs::write(vault.join("drawing.md"), note).expect("write the drawing");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        // The note's own text is still indexed…
        let hits = search_workspace(dir.path(), "cardtoken", 10).expect("search");
        assert!(
            hits.hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:drawing.md"),
            "the text elements are the note's content: {hits:?}"
        );
        // …and the scene is not, in the index or in a chunk.
        let scene = search_workspace(dir.path(), "SCENENOISEtoken", 10).expect("search");
        assert!(
            scene.hits.is_empty(),
            "the scene reached the index: {scene:?}"
        );
        let chunks = store_of(dir.path()).all_current_chunks().expect("chunks");
        for chunk in &chunks {
            assert!(
                !chunk.text.contains("SCENENOISEtoken"),
                "the scene reached a chunk: {:?}",
                chunk.text
            );
        }

        // The file was indexed, and it says what it lost.
        let row = store_of(dir.path())
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:drawing.md"))
            .expect("a coverage row for the drawing");
        assert_eq!(row.outcome.as_deref(), Some("indexed"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("excalidraw_detected"),
            "the spelling a reader branches on: {row:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::ExcalidrawDetected)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "{coverage:?}"
        );
    }

    /// One file can lose more than one thing, and its row has to say so: this is the
    /// case the list-shaped slot exists for (§5 第 8 条), and the row's `data_json` is
    /// what a reader counts from. A reviewer measured three surviving mutants with no
    /// such case in the suite — counting from the primary code alone, emptying the
    /// list, and the refresh path falling back to an empty one.
    #[test]
    fn a_file_that_loses_two_things_carries_both_on_its_row() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        // A block that cannot be read (prose between two rules) *and* a scene fence:
        // both are losses, and neither stops the file from being indexed.
        let note = "---\nThis is prose, not a field.\n---\n\n## Drawing\n```compressed-json\nSCENENOISEtoken\n```\n";
        fs::write(vault.join("drawing.md"), note).expect("write the note");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let row = |batch: &str| {
            store
                .store_events_for_batch(batch)
                .expect("read the batch")
                .into_iter()
                .find(|event| event.source_key.as_deref() == Some("note:drawing.md"))
                .expect("a coverage row for the note")
        };
        let first = row(&report.batch_id);
        assert_eq!(first.outcome.as_deref(), Some("indexed"), "{first:?}");
        assert_eq!(
            first.reason_code.as_deref(),
            Some("frontmatter_parse_error"),
            "the row's code is the list's first, in enum order: {first:?}"
        );
        let data: serde_json::Value =
            serde_json::from_str(&first.data_json).expect("the row's data is JSON");
        assert_eq!(
            data["degradations"],
            serde_json::json!(["frontmatter_parse_error", "excalidraw_detected"]),
            "both losses, in enum order: {first:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::FrontmatterParseError)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "{coverage:?}"
        );
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::ExcalidrawDetected)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "counted from the whole list, not from the first code: {coverage:?}"
        );

        // The refresh path derives the same list: moving the vault keeps every byte
        // (the mtime included), so the file is refreshed rather than re-indexed.
        let moved = dir.path().join("notes").join("vault");
        fs::create_dir_all(moved.parent().expect("parent path")).expect("create parent");
        fs::rename(&vault, &moved).expect("move the vault");
        write_config(dir.path(), &moved);
        let report = index_workspace(dir.path(), None).expect("reindex");
        let refreshed = row(&report.batch_id);
        assert_eq!(
            refreshed.outcome.as_deref(),
            Some("metadata_refreshed"),
            "a moved file is a refresh: {refreshed:?}"
        );
        assert_eq!(
            refreshed.reason_code.as_deref(),
            Some("frontmatter_parse_error"),
            "{refreshed:?}"
        );
        let data: serde_json::Value =
            serde_json::from_str(&refreshed.data_json).expect("the row's data is JSON");
        assert_eq!(
            data["degradations"],
            serde_json::json!(["frontmatter_parse_error", "excalidraw_detected"]),
            "the refresh reports what the indexing path would: {refreshed:?}"
        );
    }

    #[test]
    fn an_attachment_is_registered_without_becoming_searchable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("attach")).expect("create vault");
        // Not valid UTF-8: an attachment's bytes are not text, and nothing may
        // pretend to normalize them.
        fs::write(
            vault.join("attach/pic.png"),
            [0x89, b'P', b'N', b'G', 0xff, 0xfe, 0x00],
        )
        .expect("write the attachment");
        fs::write(vault.join("note.md"), "# Note\n\nbody\n").expect("write the note");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let revision = store
            .current_revision("note:attach/pic.png")
            .expect("read")
            .expect("the attachment is a source");
        assert_eq!(revision.kind, momotaro_contracts::SourceKind::Attachment);
        assert_eq!(
            revision.origin_class,
            momotaro_contracts::OriginClass::Owner
        );
        assert_eq!(
            revision.title.as_deref(),
            Some("pic"),
            "the title is the file's name without its extension"
        );
        assert!(
            revision
                .local_path
                .as_deref()
                .is_some_and(|path| path.ends_with("attach/pic.png")),
            "{revision:?}"
        );
        assert!(
            revision.metadata_json.contains("size_bytes"),
            "the fingerprint is the blob: {}",
            revision.metadata_json
        );
        // Zero chunks, and the file's name is not a hit.
        assert!(
            store
                .all_current_chunks()
                .expect("chunks")
                .iter()
                .all(|chunk| chunk.source_key != "note:attach/pic.png")
        );
        assert!(
            search_workspace(dir.path(), "pic", 10)
                .expect("search")
                .hits
                .iter()
                .all(|hit| hit.hit.source_key != "note:attach/pic.png"),
            "nothing was indexed for the attachment"
        );
        // The row says what happened, and the report counts it.
        let row = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:attach/pic.png"))
            .expect("a coverage row for the attachment");
        assert_eq!(row.outcome.as_deref(), Some("indexed"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("attachment_unindexed"),
            "{row:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::AttachmentUnindexed)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "{coverage:?}"
        );

        // Re-indexing skips it by fingerprint like any other unchanged source — the
        // note is unchanged too, so both are skipped, and a skipped row still says what
        // the file is (a reader that only saw the counts would not know).
        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(report.files_skipped, 2, "{report:?}");
        assert_eq!(report.revisions_created, 0, "{report:?}");
        let skipped_row = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:attach/pic.png"))
            .expect("a row for the skipped attachment");
        assert_eq!(
            skipped_row.outcome.as_deref(),
            Some("skipped"),
            "{skipped_row:?}"
        );
        assert_eq!(
            skipped_row.reason_code.as_deref(),
            Some("attachment_unindexed"),
            "a skipped row carries the code too: {skipped_row:?}"
        );
        // …and the batch counts it on the skipped axis, which is why the report needs
        // two: this same code is on the in-index axis for the batch before it.
        let reindex_coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            reindex_coverage
                .degradations
                .get(&momotaro_contracts::Degradation::AttachmentUnindexed)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((0, 1)),
            "{reindex_coverage:?}"
        );
        let current = store
            .current_revision("note:attach/pic.png")
            .expect("read")
            .expect("still there");
        assert_eq!(current.kind, momotaro_contracts::SourceKind::Attachment);
        let before_hash = current.revision_hash.clone();

        // New bytes are a new revision, not a skip and not a refresh: the file's own
        // content is the source, so replacing it has to land in the store.
        fs::write(
            vault.join("attach/pic.png"),
            [0x89, b'P', b'N', b'G', 0x00, 0x01, 0x02, 0x03],
        )
        .expect("replace the attachment");
        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(report.revisions_created, 1, "{report:?}");
        let replaced = store
            .current_revision("note:attach/pic.png")
            .expect("read")
            .expect("current");
        assert_ne!(
            replaced.revision_hash, before_hash,
            "new bytes are a new revision"
        );
        assert_eq!(replaced.kind, momotaro_contracts::SourceKind::Attachment);

        // Same bytes under a fresh fingerprint (a `touch`, a copy, a sync that rewrote
        // the mtime) is a refresh: the revision already holds them, so the mutable
        // columns move and nothing is appended — the count is what a reader believes.
        {
            let mut store = store_of(dir.path());
            store
                .update_revision_fingerprint(
                    "note:attach/pic.png",
                    &replaced.revision_hash,
                    "{\"mtime_ms\":1}",
                    "stale/away.png",
                    "away.png",
                )
                .expect("clobber the mutable columns");
        }
        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(
            report.revisions_created, 0,
            "the bytes did not change: {report:?}"
        );
        let refreshed = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:attach/pic.png"))
            .expect("a row for the refreshed attachment");
        assert_eq!(
            refreshed.outcome.as_deref(),
            Some("metadata_refreshed"),
            "a stale fingerprint is a refresh, not a new revision: {refreshed:?}"
        );
        assert_eq!(
            refreshed.reason_code.as_deref(),
            Some("attachment_unindexed"),
            "{refreshed:?}"
        );
        let stored = store
            .current_revision("note:attach/pic.png")
            .expect("read")
            .expect("current");
        assert!(
            stored
                .local_path
                .as_deref()
                .is_some_and(|path| path.ends_with("attach/pic.png")),
            "the refresh repairs the stored path: {stored:?}"
        );
    }

    /// A link is not content: the row says it was seen, and the bytes behind it
    /// never enter the index (this version does not follow them).
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_recorded_not_followed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("real.md"), "# Real\n\nrealtoken\n").expect("write");
        let outside = dir.path().join("outside.md");
        fs::write(&outside, "# Outside\n\noutsidetoken\n").expect("write outside");
        std::os::unix::fs::symlink(&outside, vault.join("link.md")).expect("create the symlink");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        assert!(
            store
                .current_revision("note:link.md")
                .expect("read")
                .is_none(),
            "a link is not a source"
        );
        let row = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:link.md"))
            .expect("a row for the link");
        assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("symlink_encountered"),
            "{row:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::SymlinkEncountered)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((0, 1)),
            "a skipped row's code is counted too: {coverage:?}"
        );
        // The link points outside the vault, so nothing behind it is indexed.
        assert!(
            search_workspace(dir.path(), "outsidetoken", 10)
                .expect("search")
                .hits
                .is_empty()
        );
    }

    /// Windows' junction is a symlink to `file_type()`, needs no privilege, and is
    /// the shape a user actually makes here: it must be recorded, and the walk must
    /// not descend into it (no file is indexed twice).
    #[cfg(windows)]
    #[test]
    fn a_junction_is_recorded_not_walked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("sub")).expect("create vault");
        fs::write(vault.join("sub/inner.md"), "# Inner\n\ninnertoken\n").expect("write");
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(vault.join("link"))
            .arg(vault.join("sub"))
            .status()
            .expect("run mklink");
        assert!(
            status.success(),
            "this test needs a junction; a silent skip would make \"did not run\" \
             look exactly like \"passed\""
        );
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        // A link is skipped, and the summary has to say so: without this the counts do
        // not add up (the note is indexed, the link is nowhere) and a reader of
        // `indexed 1 of 2 files` has no way to see why.
        assert_eq!(report.files_scanned, 2, "{report:?}");
        assert_eq!(report.files_skipped, 1, "{report:?}");
        assert_eq!(report.files_indexed, 1, "{report:?}");

        let store = store_of(dir.path());
        let row = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:link"))
            .expect("a row for the junction");
        assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("symlink_encountered"),
            "{row:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::SymlinkEncountered)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((0, 1)),
            "a skipped row's code is counted too: {coverage:?}"
        );
        assert!(
            store
                .current_revision("note:sub/inner.md")
                .expect("read")
                .is_some(),
            "the note behind the junction is indexed once, under its real path"
        );
        assert!(
            store
                .current_revision("note:link/inner.md")
                .expect("read")
                .is_none(),
            "the walk does not descend into the junction"
        );
        assert_eq!(report.revisions_created, 1, "{report:?}");
    }

    #[test]
    fn a_json_block_under_another_heading_is_still_indexed() {
        // The counterexample that keeps the scene rule anchored: the plugin's own
        // regex reads the first fence after a `##? Drawing` heading, so a JSON block
        // anywhere else is ordinary note text and must stay searchable.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(
            vault.join("appendix.md"),
            "# Notes\n\n## Appendix\n\n```json\n{\"plain\": \"keepthisjson\"}\n```\n",
        )
        .expect("write the note");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        let hits = search_workspace(dir.path(), "keepthisjson", 10).expect("search");
        assert!(
            hits.hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:appendix.md"),
            "a JSON block outside the drawing section is content: {hits:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert!(
            coverage.degradations.is_empty(),
            "and nothing was kept out of the index: {coverage:?}"
        );
    }

    /// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, the one bit every cloud
    /// placeholder carries (pinned by the pure predicate test in
    /// `momotaro-ingest`, and set by kernel-mode callers only — which is why
    /// these fixtures are injected rather than built).
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    #[test]
    fn a_cloud_placeholder_is_never_read_and_gets_a_skipped_row() {
        // D42, driven through the seam `index_workspace_with` exists for: no
        // platform here can fabricate a real placeholder, so the attributes are
        // injected and the assertions pin what being one means — nothing is
        // read, no revision appears, the row says why, and the very next run
        // (with the files readable) indexes them.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("note.md"), "# Note\n\nPLACEHOLDERTOKEN body\n").expect("write");
        fs::write(vault.join("pic.png"), "not really a png").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report =
            index_workspace_with(dir.path(), None, |_| Ok(RECALL_ON_DATA_ACCESS)).expect("index");

        assert_eq!(report.files_scanned, 2, "{report:?}");
        assert_eq!(report.files_skipped, 2, "{report:?}");
        assert_eq!(report.files_indexed, 0, "{report:?}");
        assert_eq!(report.revisions_created, 0, "{report:?}");
        let store = store_of(dir.path());
        for key in ["note:note.md", "note:pic.png"] {
            assert!(
                store.current_revision(key).expect("read").is_none(),
                "{key} must have no revision: nothing was read"
            );
            let row = store
                .store_events_for_batch(&report.batch_id)
                .expect("read the batch")
                .into_iter()
                .find(|event| event.source_key.as_deref() == Some(key))
                .expect("a coverage row");
            assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
            assert_eq!(
                row.reason_code.as_deref(),
                Some("onedrive_placeholder"),
                "{row:?}"
            );
        }
        assert!(
            store.all_current_chunks().expect("chunks").is_empty(),
            "a placeholder leaves no chunks behind"
        );
        let hits = search_workspace(dir.path(), "PLACEHOLDERTOKEN", 10).expect("search");
        assert!(
            hits.hits.is_empty(),
            "its bytes are not searchable: {hits:?}"
        );
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::OneDrivePlaceholder)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((0, 2)),
            "{coverage:?}"
        );

        // Once the bytes are here (no attributes, plain run), the same files go
        // through the ordinary path: the check runs every time, so nothing had
        // to be cached or un-done.
        let report = index_workspace(dir.path(), None).expect("index again");
        assert_eq!(report.files_indexed, 2, "{report:?}");
        assert_eq!(report.files_skipped, 0, "{report:?}");
        let hits = search_workspace(dir.path(), "PLACEHOLDERTOKEN", 10).expect("search");
        assert!(
            hits.hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:note.md"),
            "the note is searchable once its bytes are on the device: {hits:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_placeholder_is_not_read_even_when_reading_would_fail() {
        // The fixture no attribute injection can fake: metadata is readable but
        // the content is not (mode 000), so an implementation that read before
        // checking would turn this row into an error. CI's ubuntu runners are
        // not root; a root build cannot deny itself the read and has nothing
        // this fixture could prove.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        let file = vault.join("guard.md");
        fs::write(&file, "# Guard\n\nbody\n").expect("write");
        let mut perms = fs::metadata(&file).expect("stat").permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&file, perms).expect("chmod 000");
        if fs::read(&file).is_ok() {
            return;
        }
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report =
            index_workspace_with(dir.path(), None, |_| Ok(RECALL_ON_DATA_ACCESS)).expect("index");
        assert_eq!(report.errors.len(), 0, "{report:?}");
        assert_eq!(report.files_skipped, 1, "{report:?}");
        let store = store_of(dir.path());
        let row = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch")
            .into_iter()
            .find(|event| event.source_key.as_deref() == Some("note:guard.md"))
            .expect("a coverage row");
        assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("onedrive_placeholder"),
            "{row:?}"
        );
    }

    #[test]
    fn an_icloud_stub_is_reported_skipped_and_never_read() {
        // D42: the stub is not content — the note it stands for is not on this
        // device. It stayed out of the index all along (it is a dot file); what
        // is new is the row, and the row is the whole deliverable.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("note.md"), "# Note\n\nZEBRATOKEN body\n").expect("write");
        fs::write(vault.join(".note.md.icloud"), "ZEBRATOKEN from the stub\n").expect("write");
        fs::write(vault.join("x.icloud.md"), "# Guard\n\nGUARDTOKEN note\n").expect("write");
        fs::write(vault.join(".x.icloud.md"), "a hidden note\n").expect("write");
        fs::write(vault.join(".hidden.md"), "a hidden note\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        let row = rows
            .iter()
            .find(|event| event.source_key.as_deref() == Some("note:.note.md.icloud"))
            .expect("a coverage row for the stub");
        assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("icloud_placeholder"),
            "{row:?}"
        );
        assert!(
            row.data_json.contains("\"warnings\""),
            "the code list rides the payload: {}",
            row.data_json
        );
        // The hidden notes produce no rows at all — a stub is the only dot file
        // the walk keeps.
        for key in ["note:.x.icloud.md", "note:.hidden.md"] {
            assert!(
                !rows
                    .iter()
                    .any(|event| event.source_key.as_deref() == Some(key)),
                "{key} is not vault content"
            );
        }

        // The stub's bytes are not searchable; the note it stands beside is, and
        // a note whose *name* contains `.icloud` is still a note.
        let hits = search_workspace(dir.path(), "ZEBRATOKEN", 10).expect("search");
        assert!(
            hits.hits
                .iter()
                .all(|hit| hit.hit.source_key != "note:.note.md.icloud"),
            "{hits:?}"
        );
        assert!(
            hits.hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:note.md"),
            "the real note answers the same token: {hits:?}"
        );
        assert!(
            search_workspace(dir.path(), "GUARDTOKEN", 10)
                .expect("search")
                .hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:x.icloud.md"),
            "a name containing .icloud is not a stub"
        );

        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::IcloudPlaceholder)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((0, 1)),
            "{coverage:?}"
        );
        // And the human line says it, without borrowing the other state's words.
        let line = format_coverage(&coverage);
        assert!(line.contains("1 x icloud_placeholder"), "{line}");
        assert!(
            !line.contains("icloud_placeholder (1 indexed anyway)"),
            "{line}"
        );
    }

    #[test]
    fn a_name_windows_will_not_accept_is_indexed_with_a_warning() {
        // D42: the name is what syncs, so the warning is platform-independent.
        // The reserved device names here are creatable on both legs through
        // std (measured), and the negatives are exactly the false positives a
        // prefix rule would produce.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        for name in ["con.md", "aux.md", "nul.txt"] {
            fs::write(vault.join(name), format!("# {name}\n\nWINDOWSATOKEN\n")).expect("write");
        }
        // A component that is not the file name: only a nested path can tell
        // "every component of the vault-relative name" apart from "the file
        // name alone". `con` is creatable as a directory through std on both
        // legs (measured).
        fs::create_dir_all(vault.join("con")).expect("create the con dir");
        fs::write(vault.join("con/note.md"), "# inner\n\nNESTEDTOKEN\n").expect("write");
        for name in [
            "constitution.md",
            "console.md",
            "aux2.md",
            "com10.md",
            "trail .md",
        ] {
            fs::write(vault.join(name), format!("# {name}\n\nCLEANTOKEN\n")).expect("write");
        }
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");
        assert!(report.errors.is_empty(), "{report:?}");

        let store = store_of(dir.path());
        let rows = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        for name in ["con.md", "aux.md"] {
            let key = format!("note:{name}");
            let row = rows
                .iter()
                .find(|event| event.source_key.as_deref() == Some(key.as_str()))
                .expect("a row");
            assert_eq!(row.outcome.as_deref(), Some("indexed"), "{row:?}");
            assert_eq!(
                row.reason_code.as_deref(),
                Some("windows_incompatible_name"),
                "{row:?}"
            );
            assert_eq!(
                row.data_json,
                format!(
                    "{{\"warnings\":[\"windows_incompatible_name\"],\"windows_incompatible_names\":[\"{name}\"]}}"
                ),
                "the row names exactly the offending component: {row:?}"
            );
        }
        // The attachment takes the other path — registered, code says so — and
        // its row carries both vocabularies at once: the degradation first in
        // the column, the warning beside it in the payload.
        let row = rows
            .iter()
            .find(|event| event.source_key.as_deref() == Some("note:nul.txt"))
            .expect("a row");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("attachment_unindexed"),
            "a degradation outranks a warning in the column: {row:?}"
        );
        assert!(
            row.data_json.contains("\"warnings\"")
                && row.data_json.contains("windows_incompatible_names"),
            "{row:?}"
        );
        assert_eq!(
            row.data_json,
            "{\"degradations\":[\"attachment_unindexed\"],\"warnings\":[\"windows_incompatible_name\"],\"windows_incompatible_names\":[\"nul.txt\"]}",
            "both vocabularies and the component, exactly: {row:?}"
        );
        // The nested row names the directory component, not the file name.
        let row = rows
            .iter()
            .find(|event| event.source_key.as_deref() == Some("note:con/note.md"))
            .expect("a row for the nested file");
        assert_eq!(row.outcome.as_deref(), Some("indexed"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("windows_incompatible_name"),
            "{row:?}"
        );
        assert_eq!(
            row.data_json,
            "{\"warnings\":[\"windows_incompatible_name\"],\"windows_incompatible_names\":[\"con\"]}",
            "the offending component is the directory: {row:?}"
        );
        assert!(
            search_workspace(dir.path(), "NESTEDTOKEN", 10)
                .expect("search")
                .hits
                .iter()
                .any(|hit| hit.hit.source_key == "note:con/note.md"),
            "a nested file is indexed despite its directory's name"
        );
        for name in [
            "constitution.md",
            "console.md",
            "aux2.md",
            "com10.md",
            "trail .md",
        ] {
            let key = format!("note:{name}");
            let row = rows
                .iter()
                .find(|event| event.source_key.as_deref() == Some(key.as_str()))
                .expect("a row");
            assert_eq!(row.reason_code.as_deref(), None, "{row:?}");
            assert_eq!(row.data_json, "{}", "{row:?}");
        }
        // The flagged notes are still indexed and searchable: the warning does
        // not change the outcome. (`nul.txt`'s bytes are not searchable for the
        // other reason — it is an attachment, and its row says so.)
        let hits = search_workspace(dir.path(), "WINDOWSATOKEN", 10).expect("search");
        for key in ["note:con.md", "note:aux.md"] {
            assert!(
                hits.hits.iter().any(|hit| hit.hit.source_key == key),
                "{key} is searchable despite its name: {hits:?}"
            );
        }
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::WindowsIncompatibleName)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((4, 0)),
            "{coverage:?}"
        );
        assert!(coverage.failures.is_empty(), "{coverage:?}");

        // A no-op re-run keeps the warning on the skipped rows — the name is a
        // fact about the file whether or not this run read its bytes.
        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(report.files_skipped, 9, "{report:?}");
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::WindowsIncompatibleName)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((0, 4)),
            "{coverage:?}"
        );
        let rows = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        let row = rows
            .iter()
            .find(|event| event.source_key.as_deref() == Some("note:con.md"))
            .expect("a row");
        assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("windows_incompatible_name"),
            "{row:?}"
        );

        // The third read-state: a warning must ride a `metadata_refreshed` row
        // too. Rewriting the same bytes with a fresh mtime fails the fingerprint
        // check but not the hash check — that is the refresh path — and the
        // warning has to survive it. (A move *within* the vault is not this
        // case: the key is the path, so that is a new source.)
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(vault.join("aux.md"), "# aux.md\n\nWINDOWSATOKEN\n").expect("rewrite");
        let report = index_workspace(dir.path(), None).expect("index after the rewrite");
        assert_eq!(report.files_indexed, 1, "{report:?}");
        assert_eq!(report.files_skipped, 8, "{report:?}");
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::WindowsIncompatibleName)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 3)),
            "a refreshed row is in the index, and its name warning rides it: {coverage:?}"
        );
        let rows = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        let row = rows
            .iter()
            .find(|event| event.source_key.as_deref() == Some("note:aux.md"))
            .expect("a row for the rewritten file");
        assert_eq!(
            row.outcome.as_deref(),
            Some("metadata_refreshed"),
            "{row:?}"
        );
        assert_eq!(
            row.reason_code.as_deref(),
            Some("windows_incompatible_name"),
            "{row:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn illegal_characters_and_trailing_marks_survive_a_real_filesystem() {
        // Names only POSIX file systems hold at all — Win32 refuses `|` and
        // silently strips a trailing dot or space — so this half of the
        // predicate's story runs where those names are real: CI's ubuntu leg
        // (a machine that cannot create them also cannot compile this test).
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("dir.")).expect("create dir.");
        fs::create_dir_all(vault.join("dir ")).expect("create dir ");
        fs::write(vault.join("a|b.md"), "# pipe\n\nPIPETOKEN\n").expect("write");
        fs::write(vault.join("dir./note.md"), "# dot\n\nDOTTOKEN\n").expect("write");
        fs::write(vault.join("dir /note.md"), "# space\n\nSPACETOKEN\n").expect("write");
        // Two warnings on one row — an eviction stub whose name is also one
        // Windows reserves — only a POSIX file system can spell this.
        fs::write(vault.join(".a|b.md.icloud"), "stub text\n").expect("write the stub");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");
        assert!(report.errors.is_empty(), "{report:?}");

        let store = store_of(dir.path());
        let rows = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        for (key, component) in [
            ("note:a|b.md", "a|b.md"),
            ("note:dir./note.md", "dir."),
            ("note:dir /note.md", "dir "),
        ] {
            let row = rows
                .iter()
                .find(|event| event.source_key.as_deref() == Some(key))
                .expect("a row");
            assert_eq!(row.outcome.as_deref(), Some("indexed"), "{row:?}");
            assert_eq!(
                row.reason_code.as_deref(),
                Some("windows_incompatible_name"),
                "{row:?}"
            );
            assert_eq!(
                row.data_json,
                format!(
                    "{{\"warnings\":[\"windows_incompatible_name\"],\"windows_incompatible_names\":[\"{component}\"]}}"
                ),
                "the offending component, exactly: {row:?}"
            );
        }
        for (token, key) in [
            ("PIPETOKEN", "note:a|b.md"),
            ("DOTTOKEN", "note:dir./note.md"),
            ("SPACETOKEN", "note:dir /note.md"),
        ] {
            assert!(
                search_workspace(dir.path(), token, 10)
                    .expect("search")
                    .hits
                    .iter()
                    .any(|hit| hit.hit.source_key == key),
                "{key} is indexed despite its name"
            );
        }

        // The two-warning row: the stub is skipped, and the *declaration*
        // order decides both the primary code and the payload list (the
        // contracts test pins `ALL` to that order; this pins what a row does
        // with it).
        let row = rows
            .iter()
            .find(|event| event.source_key.as_deref() == Some("note:.a|b.md.icloud"))
            .expect("a row for the two-warning stub");
        assert_eq!(row.outcome.as_deref(), Some("skipped"), "{row:?}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("icloud_placeholder"),
            "the first warning in declaration order is the row's primary: {row:?}"
        );
        assert_eq!(
            row.data_json,
            "{\"warnings\":[\"icloud_placeholder\",\"windows_incompatible_name\"],\"windows_incompatible_names\":[\".a|b.md.icloud\"]}",
            "one row, two warnings, in the pinned order: {row:?}"
        );
    }

    #[test]
    fn a_warning_code_written_only_to_the_column_is_counted_not_failed() {
        // The trap this unit had to land with: a row from a writer that filled
        // no payload lists — the shape every row had before the lists existed —
        // carries its code in `reason_code` alone, and the reader must know the
        // warning vocabulary before it calls anything unknown. Reverting the
        // two-vocabulary fallback reports a warning as a failure.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("note.md"), "# Note\n\nbody\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let mut store = store_of(dir.path());
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 2,
                batch_id: &report.batch_id,
                seq: 98,
                domain: "ingest",
                event_type: "ingest.coverage",
                source_key: Some("note:from-a-writer.md"),
                outcome: Some("indexed"),
                reason_code: Some("windows_incompatible_name"),
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append a column-only row");
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::WindowsIncompatibleName)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "a known warning in the column is counted, not failed: {coverage:?}"
        );
        assert!(coverage.failures.is_empty(), "{coverage:?}");

        // A code neither vocabulary knows is still a failure: only the set of
        // known vocabularies grew, the posture did not change.
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 3,
                batch_id: &report.batch_id,
                seq: 99,
                domain: "ingest",
                event_type: "ingest.coverage",
                source_key: Some("note:from-the-future.md"),
                outcome: Some("indexed"),
                reason_code: Some("a_code_from_the_future"),
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append a foreign row");
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(coverage.failures.len(), 1, "{coverage:?}");
        assert_eq!(
            coverage.failures[0].reason_code.as_deref(),
            Some("a_code_from_the_future"),
            "{coverage:?}"
        );
        assert_eq!(
            coverage
                .warnings
                .get(&momotaro_contracts::Warning::WindowsIncompatibleName)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "{coverage:?}"
        );

        // The other half of the fallback: a column-only row whose code is a
        // DEGRADATION must land in the degradation bucket — a reader that
        // tried the warning vocabulary first would count it as a failure.
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 4,
                batch_id: &report.batch_id,
                seq: 100,
                domain: "ingest",
                event_type: "ingest.coverage",
                source_key: Some("note:older-writer.md"),
                outcome: Some("indexed"),
                reason_code: Some("frontmatter_parse_error"),
                message: None,
                data_json: "{}",
                external_id: None,
            })
            .expect("append a degradation column-only row");
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::FrontmatterParseError)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "a known degradation in the column is counted, not failed: {coverage:?}"
        );
        assert_eq!(coverage.failures.len(), 1, "{coverage:?}");
    }

    // ---- W2.4: wikilink edge building (`§5 第 2–4 条`, ADR 0027) ----

    /// Reads one file's `ingest.coverage` row of the batch: outcome, primary
    /// code, and the `data_json` payload.
    fn coverage_row_of(
        report: &momotaro_contracts::IndexReport,
        store: &Store,
        key: &str,
    ) -> (String, String, String) {
        let rows = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        let row = rows
            .iter()
            .rev()
            .find(|event| event.source_key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("a coverage row for {key}"));
        (
            row.outcome.clone().unwrap_or_default(),
            row.reason_code.clone().unwrap_or_default(),
            row.data_json.clone(),
        )
    }

    #[test]
    fn a_narrowed_walk_leaves_out_of_tree_graph_rows_alone() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("sub")).expect("create dirs");
        fs::write(vault.join("a.md"), "links [[sub/b]]\n").expect("write a");
        fs::write(vault.join("sub/b.md"), "links [[missing]]\n").expect("write b");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("full index");
        let before = store_of(dir.path()).all_graph_links().expect("rows");
        assert_eq!(before.len(), 2, "{before:?}");

        // A narrowed walk sees only `sub` — under its own vault root — and its
        // namespace is not the vault's truth: every row of the tree it did not
        // walk must survive the run (a rebuild reproduces all of them).
        index_workspace(dir.path(), Some(&vault.join("sub"))).expect("narrow index");
        let after = store_of(dir.path()).all_graph_links().expect("rows");
        for row in &before {
            assert!(
                after.contains(row),
                "a narrowed walk prunes nothing: {row:?} missing from {after:?}"
            );
        }
    }

    #[test]
    fn graph_links_build_dangling_and_rebuild_identically() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("target.md"), "# Head\n\nbody\n").expect("write target");
        fs::write(vault.join("a.md"), "links [[target]] and [[missing]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        assert_eq!(rows.len(), 2, "one edge and one dangling row: {rows:?}");
        let resolved = rows
            .iter()
            .find(|row| row.raw_target == "target")
            .expect("the resolved row");
        assert_eq!(resolved.to_source_key.as_deref(), Some("note:target.md"));
        let dangling = rows
            .iter()
            .find(|row| row.raw_target == "missing")
            .expect("the dangling row");
        assert_eq!(
            dangling.to_source_key, None,
            "dangling is NULL, not dropped"
        );
        assert_eq!(dangling.raw_target, "missing", "and keeps the writing");
        // `created_at` is the revision's `ingested_at`, not a build clock
        // (ADR 0027) — asserted against the revision row directly, because two
        // runs inside one wall-clock second cannot tell a clock from a constant.
        let revision = store
            .current_revision("note:a.md")
            .expect("read")
            .expect("a revision");
        for row in &rows {
            assert_eq!(
                row.created_at, revision.ingested_at,
                "created_at is the revision's, not the writer's clock: {row:?}"
            );
        }

        // And the code survives the summarizer: a reader of the coverage report
        // sees the loss, not a silent success.
        let coverage = coverage_report(dir.path()).expect("read").expect("a batch");
        assert_eq!(
            coverage
                .degradations
                .get(&momotaro_contracts::Degradation::UnresolvedWikilink)
                .map(|counts| (counts.in_index, counts.skipped)),
            Some((1, 0)),
            "{coverage:?}"
        );

        // The batch summary says how the graph stands (§5 第 7 条 「覆盖了多少」):
        // rows, dangling, and this run's ambiguous/stale decisions.
        let events = store
            .store_events_for_batch(&report.batch_id)
            .expect("read the batch");
        let finished = events
            .iter()
            .find(|event| event.event_type == "ingest.finished")
            .expect("the closing event");
        let payload: serde_json::Value =
            serde_json::from_str(&finished.data_json).expect("payload is JSON");
        assert_eq!(payload["graph_rows"], 2, "{payload}");
        assert_eq!(payload["graph_dangling"], 1, "{payload}");
        assert_eq!(payload["graph_ambiguous_links"], 0, "{payload}");
        assert_eq!(payload["graph_stale_anchors"], 0, "{payload}");
        // `rm graph_links` + re-run: the table is a projection, so every row
        // comes back byte for byte — `created_at` included (ADR 0027). The
        // second run skips every file; the rows are re-projected from the
        // stored extraction regardless.
        let connection =
            rusqlite::Connection::open(database_path(dir.path())).expect("open the store file");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        connection
            .execute("DELETE FROM graph_links", [])
            .expect("rm graph_links");
        drop(connection);
        index_workspace(dir.path(), None).expect("re-index");
        let rebuilt = store_of(dir.path()).all_graph_links().expect("read rows");
        assert_eq!(
            rows, rebuilt,
            "a rebuild reproduces every row byte for byte"
        );
        // And the rebuilt rows carry the revision's stamp, not the rebuild's
        // clock — checked across a clock tick, because two runs inside one
        // second cannot tell a stamp from a clock (the resolution trap).
        let revision = store_of(dir.path())
            .current_revision("note:a.md")
            .expect("read")
            .expect("a revision");
        for row in &rebuilt {
            assert_eq!(row.created_at, revision.ingested_at, "{row:?}");
        }

        // The codes ride the file's row (§5 第 7 条): the resolved link is not
        // a loss, the dangling one is exactly one.
        let (_, code, data) = coverage_row_of(&report, &store, "note:a.md");
        assert_eq!(code, "unresolved_wikilink", "{data}");
    }

    #[test]
    fn a_bad_anchor_keeps_the_row_and_a_good_anchor_is_not_stale() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        // The link sorts before its target: on the very first run the target
        // has no blob yet when the link is processed, so an anchor check run
        // mid-batch reports `stale_anchor` for a perfectly good heading. The
        // sweep checks against settled state, so `aaa-good` must stay clean.
        fs::write(vault.join("aaa-good.md"), "[[zzz-target#Head]]\n").expect("write good");
        fs::write(vault.join("aaa-bad.md"), "[[zzz-target#NoSuch]]\n").expect("write bad");
        fs::write(vault.join("zzz-target.md"), "# Head\n\nbody\n").expect("write target");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        assert_eq!(rows.len(), 2, "both anchors keep their rows: {rows:?}");
        let (good_outcome, good_code, _) = coverage_row_of(&report, &store, "note:aaa-good.md");
        assert_eq!(good_outcome, "indexed");
        assert!(
            good_code.is_empty(),
            "a heading that exists is not stale: {good_code}"
        );
        let (_, bad_code, bad_data) = coverage_row_of(&report, &store, "note:aaa-bad.md");
        assert_eq!(bad_code, "stale_anchor", "{bad_data}");
        let payload: serde_json::Value = serde_json::from_str(&bad_data).expect("payload is JSON");
        assert_eq!(
            payload["stale_anchors"],
            serde_json::json!([{ "target": "note:zzz-target.md", "anchor": "NoSuch" }]),
            "which anchor went stale is on the record: {bad_data}"
        );

        // Re-projection keeps saying so: a skipped file's row carries the code
        // its links still cost it (§5 第 7 条).
        let report = index_workspace(dir.path(), None).expect("re-index");
        let (outcome, code, _) = coverage_row_of(&report, &store, "note:aaa-bad.md");
        assert_eq!(outcome, "skipped");
        assert_eq!(code, "stale_anchor", "a skipped row does not drop codes");
    }

    #[test]
    fn an_ambiguous_name_builds_no_edge_and_lists_its_candidates() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        for path in ["d1", "d2", "d3"] {
            fs::create_dir_all(vault.join(path)).expect("create dir");
            fs::write(vault.join(path).join("dup.md"), "dup\n").expect("write dup");
        }
        fs::write(vault.join("from.md"), "[[dup]]\n").expect("write from");
        fs::write(vault.join("d1/uses.md"), "[[dup]]\n").expect("write uses");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        assert_eq!(
            rows.len(),
            1,
            "only the same-directory one gets an edge: {rows:?}"
        );
        assert_eq!(rows[0].from_source_key, "note:d1/uses.md");
        assert_eq!(rows[0].to_source_key.as_deref(), Some("note:d1/dup.md"));

        let (outcome, code, data) = coverage_row_of(&report, &store, "note:from.md");
        assert_eq!(outcome, "indexed");
        assert_eq!(code, "ambiguous_wikilink", "{data}");
        let payload: serde_json::Value = serde_json::from_str(&data).expect("payload is JSON");
        let candidates = payload["ambiguous"][0]["candidates"]
            .as_array()
            .expect("the candidates are listed")
            .clone();
        assert_eq!(
            serde_json::Value::Array(candidates),
            serde_json::json!(["note:d1/dup.md", "note:d2/dup.md", "note:d3/dup.md"]),
            "every candidate is on the record, sorted: {data}"
        );
    }

    #[test]
    fn markdown_links_decode_decode_badly_and_dangle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("target.md"), "# Head\n\nbody\n").expect("write target");
        fs::write(
            vault.join("a.md"),
            "good [t](target.md#Head) bad [u](bad%ZZname.md) gone [v](gone.md)\n",
        )
        .expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        let mut targets: Vec<(String, Option<String>)> = rows
            .iter()
            .map(|row| (row.raw_target.clone(), row.to_source_key.clone()))
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            [
                ("gone.md".to_owned(), None),
                ("target.md".to_owned(), Some("note:target.md".to_owned()))
            ],
            "decode errors build nothing; the other two are rows: {targets:?}"
        );
        let (_, code, data) = coverage_row_of(&report, &store, "note:a.md");
        // The row's primary code is the list's first, in the enum's declaration
        // order (§5 第 8 条 ②) — `unresolved_wikilink` outranks the decode loss
        // — and the whole list rides `data_json`.
        assert_eq!(code, "unresolved_wikilink", "{data}");
        let payload: serde_json::Value = serde_json::from_str(&data).expect("payload is JSON");
        assert_eq!(
            payload["degradations"],
            serde_json::json!(["unresolved_wikilink", "markdown_link_decode_error"]),
            "both losses are on the record, in declaration order: {data}"
        );

        // A skipped file still carries the losses its links cost it: the decode
        // fact lives in the blob and re-projects every run (§5 第 5 条).
        let report = index_workspace(dir.path(), None).expect("re-index");
        let (outcome, code, data) = coverage_row_of(&report, &store, "note:a.md");
        assert_eq!(outcome, "skipped");
        let payload: serde_json::Value = serde_json::from_str(&data).expect("payload is JSON");
        assert!(
            payload["degradations"]
                .as_array()
                .expect("a list")
                .contains(&serde_json::json!("markdown_link_decode_error")),
            "a skipped row does not drop the decode loss: {data}"
        );
        let _ = code;
    }

    #[test]
    fn a_refused_syntax_is_reported_and_kept_as_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(
            vault.join("a.md"),
            "x [[#^]] y
",
        )
        .expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        assert!(
            store.all_graph_links().expect("rows").is_empty(),
            "a refused shape builds no edge"
        );
        let (_, code, data) = coverage_row_of(&report, &store, "note:a.md");
        assert_eq!(code, "unknown_link_syntax", "{data}");
        assert!(
            store
                .all_current_chunks()
                .expect("chunks")
                .iter()
                .any(|chunk| chunk.text.contains("[[#^]]")),
            "and the text stays in the chunks verbatim"
        );
    }

    #[test]
    fn frontmatter_links_embeds_and_self_loops() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("target.md"), "# Head\n\nbody\n").expect("write target");
        fs::write(
            vault.join("a.md"),
            "---\nrelated: \"[[target]]\"\n---\nembed ![[target]] self [[#Head]] twice [[target]] [[target]]\n",
        )
        .expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        assert_eq!(
            rows.len(),
            3,
            "three identities: frontmatter, body link (written twice, one row), embed — \
             and the self-loop built nothing: {rows:?}"
        );
        let frontmatter = rows
            .iter()
            .find(|row| row.link_source == momotaro_contracts::LinkSource::Frontmatter)
            .expect("the frontmatter link is singled out");
        assert_eq!(frontmatter.edge_kind, momotaro_contracts::EdgeKind::Link);
        let embed = rows
            .iter()
            .find(|row| row.edge_kind == momotaro_contracts::EdgeKind::Embed)
            .expect("the embed is singled out");
        assert_eq!(embed.link_source, momotaro_contracts::LinkSource::Embed);
        assert!(
            rows.iter()
                .all(|row| row.raw_target != "#Head" && row.raw_target != "Head"),
            "the self-loop built nothing"
        );
    }

    #[test]
    fn a_target_created_later_backfills_and_a_target_removed_goes_dangling() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "[[late]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("run 1");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        assert_eq!(
            rows[0].to_source_key, None,
            "dangling while the target is not here"
        );

        // The target appears; `a.md` is untouched and will be skipped. The
        // next run's re-projection is the backfill (§5 第 2 条).
        fs::write(vault.join("late.md"), "late\n").expect("write late");
        index_workspace(dir.path(), None).expect("run 2");
        let rows = store_of(dir.path()).all_graph_links().expect("read rows");
        assert_eq!(
            rows[0].to_source_key.as_deref(),
            Some("note:late.md"),
            "the next index backfills the edge"
        );

        // ADR 0014: the target is renamed away — the old key is gone, so the
        // edge honestly goes dangling again rather than pointing at nothing.
        fs::remove_file(vault.join("late.md")).expect("remove late");
        index_workspace(dir.path(), None).expect("run 3");
        let rows = store_of(dir.path()).all_graph_links().expect("read rows");
        assert_eq!(
            rows[0].to_source_key, None,
            "a vanished target makes the edge dangling again"
        );
    }

    #[test]
    fn a_refresh_carries_the_blob_and_the_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "[[gone]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("run 1");

        // Same bytes, new mtime: the refresh path rewrites the blob wholesale,
        // and every key the edge rebuild replays from must survive the rewrite
        // (the W2.3 lesson: a whole-blob rewrite drops un-carried keys).
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(vault.join("a.md"), "[[gone]]\n").expect("rewrite a");
        let report = index_workspace(dir.path(), None).expect("run 2");
        let (outcome, _, _) = coverage_row_of(&report, &store_of(dir.path()), "note:a.md");
        assert_eq!(outcome, "metadata_refreshed");

        let store = store_of(dir.path());
        let revision = store
            .current_revision("note:a.md")
            .expect("read")
            .expect("a revision");
        let blob: serde_json::Value =
            serde_json::from_str(&revision.metadata_json).expect("blob is JSON");
        assert!(blob.get("links").is_some(), "{blob}");
        assert!(blob.get("outline").is_some(), "{blob}");
        let rows = store.all_graph_links().expect("rows");
        assert_eq!(rows.len(), 1, "and the rows re-project from it: {rows:?}");
    }

    #[test]
    fn an_attachment_target_takes_no_anchor_kind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("doc.pdf"), b"%PDF-1.4 fake").expect("write pdf");
        fs::write(vault.join("a.md"), "see [[doc.pdf#page=2]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        let report = index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("read rows");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].to_source_key.as_deref(), Some("note:doc.pdf"));
        assert_eq!(
            rows[0].anchor_kind, None,
            "a PDF page parameter is not an anchor (第五条 4 条的附件豁免)"
        );
        assert_eq!(rows[0].anchor_raw.as_deref(), Some("page=2"), "raw kept");
        let (_, code, _) = coverage_row_of(&report, &store, "note:a.md");
        assert!(
            code != "stale_anchor",
            "and nothing is checked against page numbers"
        );
    }

    #[test]
    fn replacing_a_file_drops_the_rows_it_no_longer_writes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "[[gone]] and [[missing]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("run 1");
        let store = store_of(dir.path());
        assert_eq!(store.all_graph_links().expect("rows").len(), 2);

        // The whole from-key set is replaced, not added to: a link removed from
        // the note must take its row with it.
        fs::write(vault.join("a.md"), "[[gone]]\n").expect("rewrite a");
        index_workspace(dir.path(), None).expect("run 2");
        let rows = store_of(dir.path()).all_graph_links().expect("rows");
        assert_eq!(rows.len(), 1, "the stale row is gone: {rows:?}");
        assert_eq!(rows[0].raw_target, "gone");
    }

    #[test]
    fn an_anchor_is_not_stale_when_the_target_cannot_be_verified() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("target.md"), "# Head\n\nbody\n").expect("write target");
        fs::write(vault.join("a.md"), "[[target#NoSuch]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("run 1");

        // Take the target's outline away (a blob from before the key existed):
        // "cannot verify" is not "missing", and no code may fire.
        let connection =
            rusqlite::Connection::open(database_path(dir.path())).expect("open the store file");
        connection
            .execute(
                "UPDATE source_revisions SET metadata_json = json_remove(metadata_json, '$.outline')
                 WHERE source_key = 'note:target.md'",
                [],
            )
            .expect("strip the outline");
        drop(connection);

        let report = index_workspace(dir.path(), None).expect("run 2");
        let (outcome, code, data) = coverage_row_of(&report, &store_of(dir.path()), "note:a.md");
        assert_eq!(outcome, "skipped");
        assert_ne!(code, "stale_anchor", "unverifiable is not stale: {data}");
    }

    #[test]
    fn renaming_a_source_moves_its_rows_and_prunes_the_old() {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("create vault");
        fs::write(vault.join("a.md"), "[[gone]]\n").expect("write a");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        index_workspace(dir.path(), None).expect("run 1");
        let store = store_of(dir.path());
        let rows = store.all_graph_links().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].from_source_key, "note:a.md");

        // The source itself renames away (ADR 0014: delete + create). The old
        // key's rows must not outlive it: a rebuild would never produce them.
        fs::rename(vault.join("a.md"), vault.join("b.md")).expect("rename");
        index_workspace(dir.path(), None).expect("run 2");
        let rows = store_of(dir.path()).all_graph_links().expect("rows");
        let from: Vec<&str> = rows
            .iter()
            .map(|row| row.from_source_key.as_str())
            .collect();
        assert_eq!(
            from,
            ["note:b.md"],
            "the old source's rows are pruned: {rows:?}"
        );

        // And a rebuild agrees.
        let connection =
            rusqlite::Connection::open(database_path(dir.path())).expect("open the store file");
        connection
            .execute("DELETE FROM graph_links", [])
            .expect("rm graph_links");
        drop(connection);
        index_workspace(dir.path(), None).expect("run 3");
        let rebuilt = store_of(dir.path()).all_graph_links().expect("rows");
        assert_eq!(
            rows, rebuilt,
            "a rebuild reproduces every row byte for byte"
        );
    }

    // ------------------------------------------------------------ D40 (W2.5)

    /// One gated batch — what `momotaro watch` runs for one debounce batch.
    fn gated_batch(root: &Path, path: &Path) -> crate::index_service::BatchOutcome {
        crate::index_service::run_batch(
            root,
            None,
            momotaro_ingest::attributes_of,
            crate::index_service::BatchShape::watched(
                crate::index_service::WATCH_PLAN,
                std::collections::HashSet::from([path.to_path_buf()]),
            ),
        )
        .expect("gated batch")
    }

    /// A workspace whose files are older than its store: anything indexed in
    /// the same second it was written is *racy* (D40 §4.3, and git's own
    /// rule), which is the case that does write a row. Waiting one second
    /// makes the settled case the one this helper returns.
    fn settled_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(&vault).expect("vault");
        fs::write(vault.join("a.md"), "# A\n\nfirst body\n").expect("write");
        fs::write(vault.join("b.md"), "# B\n\nsecond body\n").expect("write");
        let root = dir.path().to_path_buf();
        write_config(&root, &vault);
        init_workspace(&root).expect("init");
        // A file indexed in the same second it was written is *racy* (D40 §4.3
        // and git's own rule), which is the case that does write a row. One
        // second later it is not, and that is the settled case these tests ask
        // for.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        index_workspace(&root, None).expect("index");
        (dir, vault, root)
    }

    #[test]
    fn a_false_event_writes_nothing_at_all() {
        // D40 §4.2 verbatim: "一致且非 racy 则丢弃假事件" — dropped means
        // nothing is written, not "a row saying nothing happened". A watcher
        // that answered every editor save with three rows would fill the log
        // with the fact that nothing changed.
        let (_dir, vault, root) = settled_workspace();
        let before = store_of(&root)
            .latest_store_events(100)
            .expect("read")
            .len();

        let outcome = gated_batch(&root, &vault.join("a.md"));
        let after = store_of(&root)
            .latest_store_events(100)
            .expect("read")
            .len();

        assert_eq!(
            outcome.report.files_scanned, 0,
            "the gate found no work to do"
        );
        assert_eq!(before, after, "a false event writes nothing at all");
    }

    #[test]
    fn a_file_the_vault_lost_is_recorded_once() {
        // D40 §4.2's delete half. Every full walk redisovers the absence, so
        // without the guard a file gone for a week would earn a row per run.
        let (dir, vault, root) = settled_workspace();
        fs::remove_file(vault.join("b.md")).expect("remove");

        let count = |store: &Store| {
            store
                .latest_store_events(100)
                .expect("read")
                .iter()
                .filter(|event| {
                    event.source_key.as_deref() == Some("note:b.md")
                        && event.reason_code.as_deref() == Some("source_removed")
                })
                .count()
        };

        index_workspace(&root, None).expect("run 2");
        assert_eq!(
            count(&store_of(&root)),
            1,
            "the first walk after the deletion records it"
        );
        let row = store_of(&root)
            .latest_store_events(100)
            .expect("read")
            .into_iter()
            .find(|event| event.reason_code.as_deref() == Some("source_removed"))
            .expect("the row exists");
        assert_eq!(row.outcome.as_deref(), Some("skipped"));
        assert_eq!(row.domain, "ingest");

        index_workspace(&root, None).expect("run 3");
        assert_eq!(
            count(&store_of(&root)),
            1,
            "rediscovered every walk, recorded once"
        );
        assert!(
            !dir.path().join("vault").join("b.md").exists(),
            "the file really is gone from the vault"
        );
    }

    #[test]
    fn a_watcher_batch_records_each_file_exactly_once() {
        // The row rides the file's own transaction (D40 §4.7), so the batch
        // tail must not write it a second time — and the batch must still be
        // an ingest batch, because `doctor --coverage` reads that domain.
        let (_dir, vault, root) = settled_workspace();
        fs::write(vault.join("a.md"), "# A\n\nsecond body changed\n").expect("write");

        let outcome = gated_batch(&root, &vault.join("a.md"));
        assert!(
            outcome.report.batch_id.starts_with("watch-"),
            "a reader can tell who wrote it: {}",
            outcome.report.batch_id
        );
        assert_eq!(
            outcome.report.revisions_created, 1,
            "the bytes were rebuilt"
        );

        let store = store_of(&root);
        let events = store
            .store_events_for_batch(&outcome.report.batch_id)
            .expect("read batch");
        let types: Vec<&str> = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec!["ingest.started", "ingest.coverage", "ingest.finished"],
            "one row per file, not two"
        );
        assert!(
            events.iter().all(|event| event.domain == "ingest"),
            "a watcher batch is an ingest batch (ADR 0026)"
        );
        let coverage = &events[1];
        assert_eq!(
            coverage.source_key.as_deref(),
            Some("note:a.md"),
            "the row names the file it is about"
        );
        assert_eq!(coverage.outcome.as_deref(), Some("indexed"));
        assert_eq!(
            outcome.removed, 0,
            "nothing was deleted, so nothing is reported as gone"
        );
    }

    #[test]
    fn a_vault_that_lost_a_file_reports_the_removal_it_found() {
        // Both existing assertions on `removed` are `== 0`, so a batch that
        // always reported zero passed every one of them. This is the other
        // side: exactly one key gone, exactly one counted.
        let (dir, vault, root) = settled_workspace();
        fs::remove_file(vault.join("b.md")).expect("remove");

        let outcome = crate::index_service::run_batch(
            &root,
            None,
            momotaro_ingest::attributes_of,
            crate::index_service::BatchShape::full(crate::index_service::RECONCILE_PLAN),
        )
        .expect("batch");
        assert_eq!(
            outcome.removed, 1,
            "the key the walk no longer names is counted once"
        );
        assert_eq!(outcome.report.files_scanned, 1, "one file is left");
        let _ = dir;
    }

    #[test]
    fn a_directory_event_opens_no_batch() {
        // A folder created in the vault is not a file to rescan. Admitting it
        // used to open a batch that claimed a scan and wrote no row — an audit
        // log asserting work on a file that does not exist.
        let (dir, vault, root) = settled_workspace();
        let before = store_of(&root)
            .latest_store_events(100)
            .expect("read")
            .len();
        fs::create_dir_all(vault.join("brand-new-folder")).expect("mkdir");

        let outcome = gated_batch(&root, &vault.join("brand-new-folder"));
        assert_eq!(
            outcome.report.files_scanned, 0,
            "a directory is not a file to scan"
        );
        assert_eq!(outcome.removed, 0, "and nothing was deleted");
        let after = store_of(&root)
            .latest_store_events(100)
            .expect("read")
            .len();
        assert_eq!(
            before, after,
            "no batch, no rows: the gate said there was nothing to do"
        );
        let _ = dir;
    }

    /// Counts `source_removed` rows for one source, straight from the log.
    fn removals_for(root: &Path, source_key: &str) -> usize {
        store_of(root)
            .latest_store_events(200)
            .expect("read")
            .iter()
            .filter(|event| {
                event.source_key.as_deref() == Some(source_key)
                    && event.reason_code.as_deref() == Some("source_removed")
            })
            .count()
    }

    #[test]
    fn a_removed_source_leaves_the_index_and_a_rebuild_does_not_bring_it_back() {
        // §4.2: "the document goes with it, in the same batch". The mutation
        // that deletes `SearchIndex::remove_source` must die here — no other
        // test could see it, because every other search runs against a file
        // that is still in the vault.
        let (dir, vault, root) = settled_workspace();
        assert!(
            search_workspace(&root, "second body", 5)
                .expect("search")
                .hits
                .iter()
                .any(|hit| hit.hit.source_key.contains("b.md")),
            "the fixture is findable before anything is deleted"
        );

        fs::remove_file(vault.join("b.md")).expect("remove");
        let outcome = crate::index_service::run_batch(
            &root,
            None,
            momotaro_ingest::attributes_of,
            crate::index_service::BatchShape::full(crate::index_service::RECONCILE_PLAN),
        )
        .expect("reconcile");
        assert_eq!(outcome.removed, 1, "one key gone");
        assert!(
            search_workspace(&root, "second body", 5)
                .expect("search")
                .hits
                .iter()
                .all(|hit| !hit.hit.source_key.contains("b.md")),
            "the document went with the source, in the same batch"
        );

        // And the derived index is a *projection*: rebuilding it from the
        // canonical store must reproduce the deletion, not undo it.
        rebuild_index(&root).expect("rebuild");
        assert!(
            search_workspace(&root, "second body", 5)
                .expect("search")
                .hits
                .iter()
                .all(|hit| !hit.hit.source_key.contains("b.md")),
            "a rebuild reads current revisions, so a demoted source stays gone"
        );
        let _ = dir;
    }

    #[test]
    fn a_restored_file_is_searchable_again_without_a_rebuild() {
        // The other half of the same guarantee: taking a source out of the
        // index has to be reversible by the ordinary path, or "eventually
        // consistent" means "until someone runs rebuild-index by hand".
        let (dir, vault, root) = settled_workspace();
        let bytes = fs::read(vault.join("b.md")).expect("read");
        fs::remove_file(vault.join("b.md")).expect("remove");
        index_workspace(&root, None).expect("index");
        assert_eq!(removals_for(&root, "note:b.md"), 1);

        fs::write(vault.join("b.md"), &bytes).expect("restore");
        index_workspace(&root, None).expect("index again");
        assert!(
            search_workspace(&root, "second body", 5)
                .expect("search")
                .hits
                .iter()
                .any(|hit| hit.hit.source_key.contains("b.md")),
            "identical bytes come back through the ordinary index path"
        );
        assert_eq!(
            removals_for(&root, "note:b.md"),
            1,
            "restoring is not a second deletion"
        );
        let _ = dir;
    }

    #[test]
    fn a_second_disappearance_is_a_second_fact() {
        // `has_row_since`'s window was a window that never reopened —
        // `ingested_at` is written by the INSERT alone — so delete → restore →
        // delete recorded the second absence not at all. `is_current` is the
        // state that actually moves, and this is what it buys.
        let (dir, vault, root) = settled_workspace();
        let bytes = fs::read(vault.join("b.md")).expect("read");

        fs::remove_file(vault.join("b.md")).expect("remove");
        index_workspace(&root, None).expect("first removal");
        assert_eq!(removals_for(&root, "note:b.md"), 1);

        fs::write(vault.join("b.md"), &bytes).expect("restore");
        index_workspace(&root, None).expect("restore");
        assert_eq!(removals_for(&root, "note:b.md"), 1, "still one");

        fs::remove_file(vault.join("b.md")).expect("remove");
        index_workspace(&root, None).expect("second removal");
        assert_eq!(
            removals_for(&root, "note:b.md"),
            2,
            "the file came back and left again: both absences are facts"
        );
        let _ = dir;
    }

    #[test]
    fn a_narrowed_walk_says_nothing_about_the_rest_of_the_vault() {
        // `index --vault <sub>` names fewer files on purpose. Treating every
        // other source as deleted emptied the search index outside the
        // override and wrote `source_removed` rows for files still on disk.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("sub")).expect("subdir");
        fs::write(vault.join("outside.md"), "# O\n\noutside body\n").expect("write");
        fs::write(vault.join("sub/inside.md"), "# I\n\ninside body\n").expect("write");
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        index_workspace(dir.path(), None).expect("full index");
        assert!(removals_for(dir.path(), "note:outside.md") == 0);

        let report = index_workspace(dir.path(), Some(&vault.join("sub"))).expect("sub index");
        assert_eq!(report.files_scanned, 1, "only the override was walked");
        assert_eq!(
            removals_for(dir.path(), "note:outside.md"),
            0,
            "a narrowed walk may not call the rest of the vault absent"
        );
        assert!(
            search_workspace(dir.path(), "outside body", 5)
                .expect("search")
                .hits
                .iter()
                .any(|hit| hit.hit.source_key.contains("outside")),
            "and the search index still has it"
        );
    }

    #[test]
    fn coverage_ignores_a_batch_that_never_opened() {
        // A `source_moved` hint rides the `ingest` domain in a batch of its own
        // with no shell. "The newest row in the domain" would report *that* as
        // the last ingest run and hand the reader an unfinished `batch_id`.
        let (dir, _vault, root) = settled_workspace();
        let report = coverage_report(&root)
            .expect("read")
            .expect("there is an ingest batch");
        assert!(
            report.batch_id.starts_with("index-"),
            "the real batch: {}",
            report.batch_id
        );

        let mut store = store_of(&root);
        store
            .append_store_event(&momotaro_store::NewStoreEvent {
                occurred_at: 1_700_000_000,
                batch_id: "move-1700000000000-1-0",
                seq: 1,
                domain: "ingest",
                event_type: "source_moved",
                source_key: Some("note:gone.md"),
                outcome: None,
                reason_code: None,
                message: None,
                data_json: r#"{"old_key":"note:a.md","new_key":"note:b.md"}"#,
                external_id: None,
            })
            .expect("hint");

        let after = coverage_report(&root)
            .expect("read")
            .expect("there is still an ingest batch");
        assert_eq!(
            after.batch_id, report.batch_id,
            "a shell-less hint does not become the last ingest run"
        );
        assert!(after.finished, "and the batch it names is finished");
        let _ = dir;
    }

    #[test]
    fn a_directory_rename_keeps_its_children_searchable() {
        // notify reports a directory rename as an event about the directory:
        // the children of the old path then look absent from the walk, and the
        // children of the new path have had no event at all. Gating a
        // directory away silently emptied the index for the whole subtree.
        let dir = tempfile::tempdir().expect("temp dir");
        let vault = dir.path().join("vault");
        fs::create_dir_all(vault.join("dir00")).expect("dir");
        for n in 0..5 {
            fs::write(
                vault.join(format!("dir00/f{n}.md")),
                format!("# F{n}\n\nZEBRAQUARK paragraph {n}\n"),
            )
            .expect("write");
        }
        write_config(dir.path(), &vault);
        init_workspace(dir.path()).expect("init");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        index_workspace(dir.path(), None).expect("index");
        let hits = |root: &Path| {
            search_workspace(root, "ZEBRAQUARK", 10)
                .expect("search")
                .hits
                .len()
        };
        assert_eq!(hits(dir.path()), 5, "all five are findable");

        fs::rename(vault.join("dir00"), vault.join("dir00x")).expect("rename");
        let outcome = gated_batch(dir.path(), &vault.join("dir00x"));
        assert!(
            outcome.report.files_scanned >= 5,
            "a directory event brings its children: {}",
            outcome.report.files_scanned
        );
        assert_eq!(
            hits(dir.path()),
            5,
            "and they are still findable after the move"
        );
        let _ = dir;
    }

    #[test]
    fn the_network_sentence_is_the_contract_copy() {
        // D31 / D40 §4.8's words are contract copy: rewording the constant must
        // break a test before it ships, exactly like a wire spelling — the
        // both-exits test below only proves the exits agree with each other.
        assert_eq!(
            NETWORK_VOLUME_NOTE,
            "this workspace is on a network volume, which Momotaro does not promise to be real-time; SQLite's WAL does not work over a network filesystem, and broken locking there can corrupt the database",
        );
    }

    #[test]
    fn the_network_sentence_is_the_same_at_both_exits() {
        // D40 §4.8's copy: one sentence, two exits. A promise that only the
        // human line makes is a promise `--json` consumers never hear.
        let report = HealthReport {
            status: WorkspaceStatus::Ready,
            schema_version: None,
            sqlite: None,
            message: None,
            network_volume: Some(true),
        };
        let human = format_health(&report);
        assert!(
            human.contains(NETWORK_VOLUME_NOTE),
            "the human line carries it: {human}"
        );
        let json_side = format_watch_line(&WatchLine::NetworkVolume {
            message: NETWORK_VOLUME_NOTE.to_owned(),
        });
        assert_eq!(
            json_side, NETWORK_VOLUME_NOTE,
            "both exits say the same words"
        );
        let local = HealthReport {
            network_volume: Some(false),
            ..report
        };
        assert!(
            !format_health(&local).contains(NETWORK_VOLUME_NOTE),
            "a local volume is not warned about"
        );
    }

    #[test]
    fn the_reconcile_line_names_every_field() {
        // `format_reconcile` is the only writer of that sentence; the JSON exit
        // serializes the same struct, so the two can only drift through here.
        let report = ReconcileReport {
            batch_id: "reconcile-1".to_owned(),
            scanned: 3,
            changed: 2,
            removed: 1,
            events: 7,
        };
        let line = format_reconcile(&report);
        for expected in [
            "reconcile-1",
            "3 scanned",
            "2 changed",
            "1 removed",
            "7 events",
        ] {
            assert!(line.contains(expected), "`{expected}` in {line}");
        }
    }

    #[test]
    fn a_reconcile_writes_its_own_family() {
        // D40 §4.4's manual trigger: the four triggers share one walk and one
        // event family, and `domain = "vault"` keeps it out of what
        // `doctor --coverage` calls an ingest batch.
        let (dir, vault, root) = settled_workspace();
        let _ = vault;
        let report = reconcile(&root).expect("reconcile");
        assert!(
            report.batch_id.starts_with("reconcile-"),
            "{}",
            report.batch_id
        );

        let store = store_of(&root);
        let events = store
            .store_events_for_batch(&report.batch_id)
            .expect("read batch");
        let types: Vec<&str> = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect();
        assert_eq!(
            types,
            vec![
                "vault.reconcile.started",
                "vault.reconcile.coverage",
                "vault.reconcile.coverage",
                "vault.reconcile.finished"
            ]
        );
        assert!(
            events.iter().all(|event| event.domain == "vault"),
            "reconciliation is its own domain"
        );
        assert_eq!(
            report.events,
            events.len() as u64,
            "the report counts what the log holds, not what the writer thought"
        );
        assert_eq!(report.scanned, 2, "both files are still there");
        assert_eq!(report.changed, 0, "and neither changed");
        assert_eq!(report.removed, 0, "nothing was deleted");
        let _ = dir;
    }

    #[test]
    fn the_gate_answers_from_a_real_store() {
        // The four answers, against a store that actually holds a row. The
        // fifth (`ForceRebuild`) needs bytes that differ behind an identical
        // stat, which no portable test can build — `verdict` carries it.
        let (dir, vault, root) = settled_workspace();
        let store = store_of(&root);
        let workspace = momotaro_ingest::canonical_workspace_root(&root).expect("canonical");
        let vault = momotaro_ingest::canonical_workspace_root(&vault).expect("canonical");

        assert_eq!(
            crate::index_service::gate(
                &store,
                &workspace,
                &vault,
                &vault.join("a.md"),
                false,
                crate::index_service::last_written(&store),
            )
            .expect("gate"),
            crate::index_service::Gate::Drop,
            "settled and unchanged: a false event"
        );
        assert_eq!(
            crate::index_service::gate(
                &store,
                &workspace,
                &vault,
                &vault.join("gone.md"),
                false,
                crate::index_service::last_written(&store),
            )
            .expect("gate"),
            crate::index_service::Gate::Absent,
            "not there any more"
        );
        fs::write(vault.join("a.md"), "# A\n\nedited while we watched\n").expect("write");
        let store = store_of(&root);
        assert_eq!(
            crate::index_service::gate(
                &store,
                &workspace,
                &vault,
                &vault.join("a.md"),
                false,
                crate::index_service::last_written(&store),
            )
            .expect("gate"),
            crate::index_service::Gate::Proceed,
            "the stat moved, so there is work"
        );
        let _ = dir;
    }
}
