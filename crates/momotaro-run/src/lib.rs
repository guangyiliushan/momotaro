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

use momotaro_contracts::{HealthReport, IndexPolicy, StatsReport, VaultScope, WorkspaceStatus};
use momotaro_ingest::IngestError;
use momotaro_retrieve::RetrieveError;
use momotaro_store::{Store, StoreError};
use serde::Deserialize;
use thiserror::Error;

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

    /// The search index has never been built for this workspace.
    #[error("search index not built; run `momotaro index` first: {0}")]
    IndexNotBuilt(String),

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

fn vault_scope(root: &Path, config: &WorkspaceConfig) -> VaultScope {
    let path = if config.vault.path.is_absolute() {
        config.vault.path.clone()
    } else {
        root.join(&config.vault.path)
    };

    VaultScope {
        root: path,
        name: config.vault.name.clone(),
    }
}

/// Initializes the local SQLite store and returns a small stats report.
pub fn init_workspace(root: impl AsRef<Path>) -> Result<StatsReport, RunError> {
    let root = root.as_ref();
    let config = load_config(root)?;
    let vault = vault_scope(root, &config);

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
    let root = root.as_ref();

    let config = match load_config(root) {
        Ok(config) => config,
        Err(RunError::ConfigRead { path, .. }) => {
            return Ok(HealthReport {
                status: WorkspaceStatus::Invalid,
                schema_version: None,
                message: Some(format!("configuration file is missing: {}", path.display())),
            });
        }
        Err(RunError::ConfigParse { source, .. }) => {
            return Ok(HealthReport {
                status: WorkspaceStatus::Invalid,
                schema_version: None,
                message: Some(format!("configuration file is invalid: {source}")),
            });
        }
        Err(error) => return Err(error),
    };

    let vault = vault_scope(root, &config);
    if !vault.root.is_dir() {
        return Ok(HealthReport {
            status: WorkspaceStatus::Invalid,
            schema_version: None,
            message: Some(format!(
                "vault path does not exist: {}",
                vault.root.display()
            )),
        });
    }

    let database_path = database_path(root);
    if !database_path.exists() {
        return Ok(HealthReport {
            status: WorkspaceStatus::Uninitialized,
            schema_version: None,
            message: Some(format!(
                "database is not initialized: {}",
                database_path.display()
            )),
        });
    }

    Ok(Store::open(database_path)?.health()?)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(hit.hit.source_key, "fourier.md");
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
            current_hash_of(&store, "fourier.md")
        };
        // Deterministic id of the old revision's first chunk (ordinal 0
        // always exists; fourier.md chunks several paragraphs).
        let old_chunk_id = momotaro_ingest::chunk_id("fourier.md", &old_hash, 0);

        let fourier = vault.join("fourier.md");
        let mut body = fs::read_to_string(&fourier).expect("read fourier");
        body.push_str("\n\n新增段落：傅里叶级数把周期函数展开为正弦与余弦的叠加。\n");
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&fourier, body).expect("append paragraph");

        let second = index_workspace(dir.path(), None).expect("second index");
        assert_eq!(second.revisions_created, 1, "only the edited file");
        assert_eq!(second.files_skipped, second.files_scanned - 1);

        let store = store_of(dir.path());
        let new_hash = current_hash_of(&store, "fourier.md");
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
        assert_eq!(old_chunk.source_key, "fourier.md");
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

        fs::remove_dir_all(dir.path().join(".momotaro/index")).expect("delete index dir");

        let error = search_workspace(dir.path(), "傅里叶", 10).expect_err("no index");
        assert!(matches!(error, RunError::IndexNotBuilt(_)));

        let rebuild = rebuild_index(dir.path()).expect("rebuild");
        assert_eq!(rebuild.chunks, before.chunks_written);

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
    fn rebuild_is_idempotent_and_counts_match_current_chunks() {
        let (dir, _vault) = fixture_workspace();

        index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let counts = store.counts().expect("counts");
        let current_chunks = store.all_current_chunks().expect("current chunks").len() as u64;
        drop(store);

        let first = rebuild_index(dir.path()).expect("rebuild once");
        let second = rebuild_index(dir.path()).expect("rebuild twice");
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

        // Wipe the fingerprint with a non-matching JSON blob so the fast
        // path must fall through to the hash check and refresh the
        // metadata instead of skipping forever.
        {
            let mut store = store_of(dir.path());
            let hash = current_hash_of(&store, "fourier.md");
            store
                .update_revision_metadata("fourier.md", &hash, "{\"mtime_ms\":0}")
                .expect("clobber fingerprint");
        }

        let report = index_workspace(dir.path(), None).expect("reindex");
        assert_eq!(report.revisions_created, 0, "hash unchanged");
        assert_eq!(report.files_indexed, 1, "metadata refreshed");

        let store = store_of(dir.path());
        let metadata = store
            .current_revision("fourier.md")
            .expect("read")
            .expect("exists")
            .metadata_json;
        assert!(
            metadata.contains("\"tool\":\"momotaro-index-v1\""),
            "fingerprint restored: {metadata}"
        );
        assert_ne!(metadata, "{\"mtime_ms\":0}");
    }

    #[test]
    fn index_workspace_with_path_override_uses_given_vault() {
        let (dir, _vault) = fixture_workspace();
        let alt = tempfile::tempdir().expect("alt vault");
        fs::write(
            alt.path().join("only.md"),
            "# Override\n\n路径覆盖傅里叶验证段落。\n",
        )
        .expect("write alt note");

        let report = index_workspace(dir.path(), Some(alt.path())).expect("override index");
        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.revisions_created, 1);

        let output = search_workspace(dir.path(), "傅里叶", 10).expect("search");
        assert!(
            output
                .hits
                .iter()
                .any(|hit| hit.hit.source_key == "only.md"),
            "override vault content must be searchable"
        );
    }

    #[test]
    fn index_titles_and_heading_paths_flow_to_search() {
        let (dir, _vault) = fixture_workspace();
        index_workspace(dir.path(), None).expect("index");

        let store = store_of(dir.path());
        let title = store
            .current_revision_title("fourier.md")
            .expect("title read")
            .expect("derived title");
        assert_eq!(title, "傅里叶变换");

        let output = search_workspace(dir.path(), "卷积", 10).expect("search");
        assert!(!output.hits.is_empty(), "zh body query must hit");
        assert!(output.hits.iter().any(|hit| hit.hit.heading_path.is_some()));
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
            .filter(|chunk| chunk.source_key == "empty.md")
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
                .any(|hit| hit.hit.source_key == "note-517.md"),
            "sampled marker must be findable"
        );
        println!(
            "perf smoke: index+search of 1000 files took {} ms (index report {} ms)",
            started.elapsed().as_millis(),
            report.duration_ms
        );
    }
}
