//! Service functions used by Momotaro surfaces.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod index_service;

pub use index_service::{
    RebuildReport, SearchHit, SearchOutput, index_workspace, rebuild_index, search_workspace,
};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use momotaro_contracts::{
    HealthReport, IndexFileError, IndexFileErrorKind, IndexPolicy, IndexReport, SchemaVersion,
    SqliteFacts, StatsReport, VaultScope, WorkspaceStatus,
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
}

#[derive(Debug, Deserialize)]
struct WorkspaceConfig {
    vault: VaultConfig,
    #[serde(default)]
    index: IndexPolicy,
}

#[derive(Debug, Deserialize)]
struct VaultConfig {
    path: PathBuf,
    #[serde(default = "default_vault_name")]
    name: String,
}

fn default_vault_name() -> String {
    "default".to_owned()
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

fn config_path(root: &Path) -> PathBuf {
    root.join(CONFIG_FILE)
}

fn data_dir(root: &Path) -> PathBuf {
    root.join(DATA_DIR)
}

fn database_path(root: &Path) -> PathBuf {
    data_dir(root).join(DATABASE_FILE)
}

fn load_config(root: &Path) -> Result<WorkspaceConfig, RunError> {
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
    let report = |status: WorkspaceStatus,
                  schema_version: Option<SchemaVersion>,
                  message: Option<String>| HealthReport {
        status,
        schema_version,
        sqlite: Some(sqlite.clone()),
        message,
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
        "indexed {} of {} files ({} new revisions, {} unchanged), {} chunks, {} errors ({} key collisions) in {} ms",
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
fn sanitize(text: &str) -> String {
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
/// not user-authored — so the text goes through [`sanitize`] and is cut to
/// [`EXCERPT_LINE_CHARS`].
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
    match report.sqlite.as_ref() {
        Some(facts) => format!("{line} (SQLite {})", facts.version),
        None => line,
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
        assert_eq!(report.files_indexed, 1, "{report:?}");
        assert_eq!(report.errors.len(), 0, "{report:?}");

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

        index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let counts = store.counts().expect("counts");
        let current_chunks = store.all_current_chunks().expect("current chunks").len() as u64;
        drop(store);

        let first = rebuild_index(dir.path()).expect("rebuild once");
        let second = rebuild_index(dir.path()).expect("rebuild twice");
        assert!(
            first.replaced_stale.is_none() && second.replaced_stale.is_none(),
            "a healthy index is not reported as replaced: {first:?}"
        );
        assert_eq!(first.chunks, second.chunks, "rebuild is idempotent");
        assert_eq!(
            first.chunks, current_chunks,
            "rebuild covers exactly current chunks"
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
}
