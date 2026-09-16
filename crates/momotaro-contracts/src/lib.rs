//! Versioned contracts shared by the Momotaro runtime surfaces.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The schema version understood by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// A versioned SQLite schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaVersion(pub u32);

impl SchemaVersion {
    /// The schema version this build can open.
    pub const CURRENT: Self = Self(CURRENT_SCHEMA_VERSION);
}

/// The local vault a run operates against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultScope {
    /// Absolute or caller-resolved vault root.
    pub root: PathBuf,
    /// Human-readable vault name.
    pub name: String,
}

/// Coarse workspace health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    /// The workspace is ready for local deterministic operations.
    Ready,
    /// The workspace has not been initialized.
    Uninitialized,
    /// The workspace is present but has a recoverable configuration problem.
    Invalid,
}

/// Durable health facts about the local workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthReport {
    /// Current workspace state.
    pub status: WorkspaceStatus,
    /// Schema version when the SQLite store is initialized.
    pub schema_version: Option<SchemaVersion>,
    /// User-facing explanation for non-ready states.
    pub message: Option<String>,
}

/// Small stats projection for the CLI `stats` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsReport {
    /// Current workspace state.
    pub status: WorkspaceStatus,
    /// Schema version when the SQLite store is initialized.
    pub schema_version: Option<SchemaVersion>,
    /// The local SQLite database path.
    pub database_path: PathBuf,
    /// Whether the database exists and is initialized.
    pub initialized: bool,
    /// Number of current source revisions, when known.
    pub sources: Option<u64>,
    /// Number of chunks across current revisions, when known.
    pub chunks: Option<u64>,
}

/// What kind of content a source revision holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// User-authored Markdown notes.
    Note,
    /// Downloaded paper content.
    Paper,
    /// Web page captures.
    Web,
}

/// Where a source revision came from and how much it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginClass {
    /// User-written notes and annotations; trusted facts.
    Owner,
    /// Paper originals; external but structured.
    Paper,
    /// Web content; untrusted.
    Web,
    /// AI-generated content; always `proposed` until confirmed.
    Agent,
    /// System-generated explanation.
    System,
}

/// One immutable version of a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRevision {
    /// Canonical identity, e.g. vault-relative path or `arxiv:NNNN.NNNNN`.
    pub source_key: String,
    /// `sha256` over normalized source bytes.
    pub revision_hash: String,
    /// Content kind.
    pub kind: SourceKind,
    /// Display title, when derivable.
    pub title: Option<String>,
    /// Remote URI, when the source has one.
    pub uri: Option<String>,
    /// Absolute local path, when the source lives on disk.
    pub local_path: Option<String>,
    /// Trust origin.
    pub origin_class: OriginClass,
    /// JSON metadata blob (fingerprint, tool tag, provider data).
    pub metadata_json: String,
    /// Unix epoch seconds at ingestion.
    pub ingested_at: i64,
    /// Whether this is the newest revision for the source key.
    pub is_current: bool,
}

/// The searchable unit derived from one source revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    /// Deterministic global identifier.
    pub chunk_id: String,
    /// Owning source key.
    pub source_key: String,
    /// Owning revision hash.
    pub revision_hash: String,
    /// Position within the revision.
    pub ordinal: u32,
    /// Heading path rendered top-down, when the chunk sits under headings.
    pub heading_path: Option<String>,
    /// Verbatim source text.
    pub text: String,
    /// JSON locator (`start_byte`/`end_byte`/`start_line`/`end_line`/`truncated`).
    pub locator_json: String,
    /// Deterministic content hash.
    pub chunk_hash: String,
    /// Estimated token count.
    pub token_estimate: u32,
}

/// A ranked search hit. Always carries the full citation key set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalHit {
    /// 1-based rank in result order.
    pub rank: u32,
    /// Canonical source identity.
    pub source_key: String,
    /// Revision the hit resolves to.
    pub revision_hash: String,
    /// Chunk identifier.
    pub chunk_id: String,
    /// Chunk content hash.
    pub chunk_hash: String,
    /// Position within the revision.
    pub ordinal: u32,
    /// Heading path, when present.
    pub heading_path: Option<String>,
    /// BM25 score as reported by the engine.
    pub score: f64,
    /// JSON locator for the chunk.
    pub locator_json: String,
}

/// Index-time policy knobs, frozen per ingest run.
///
/// Every field carries its serde default, so a config section that omits
/// any knob lands on the documented default instead of failing to parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPolicy {
    /// Lexical engine selector; only `tantivy_bm25` is supported.
    #[serde(default = "default_lexical")]
    pub lexical: String,
    /// Hybrid retrieval toggle; must stay off until a dense baseline exists.
    #[serde(default)]
    pub hybrid: bool,
    /// CJK tokenizer selector; only `cjk` (bigram) is supported.
    #[serde(default = "default_cjk_tokenizer")]
    pub cjk_tokenizer: String,
    /// Maximum estimated tokens per chunk.
    #[serde(default = "default_max_chunk_tokens")]
    pub max_chunk_tokens: u32,
    /// Overlap tokens between consecutive text chunks.
    #[serde(default = "default_chunk_overlap_tokens")]
    pub chunk_overlap_tokens: u32,
}

fn default_lexical() -> String {
    "tantivy_bm25".to_string()
}

fn default_cjk_tokenizer() -> String {
    "cjk".to_string()
}

fn default_max_chunk_tokens() -> u32 {
    512
}

fn default_chunk_overlap_tokens() -> u32 {
    64
}

impl Default for IndexPolicy {
    fn default() -> Self {
        Self {
            lexical: "tantivy_bm25".to_string(),
            hybrid: false,
            cjk_tokenizer: "cjk".to_string(),
            max_chunk_tokens: 512,
            chunk_overlap_tokens: 64,
        }
    }
}

/// Per-file failure recorded during an index run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexFileError {
    /// Source key of the file that failed.
    pub source_key: String,
    /// Structured failure message.
    pub message: String,
}

/// Aggregate outcome of one `momotaro index` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexReport {
    /// Markdown files discovered under the vault root.
    pub files_scanned: u64,
    /// Files skipped because the fingerprint was unchanged.
    pub files_skipped: u64,
    /// Files that produced a new revision or refresh.
    pub files_indexed: u64,
    /// New revisions written to canonical storage.
    pub revisions_created: u64,
    /// Chunks written (replaces count, not net-new count).
    pub chunks_written: u64,
    /// Per-file failures; the batch never aborts on a single file.
    pub errors: Vec<IndexFileError>,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
}

/// Whether `c` falls in a CJK range (Han, kana, Hangul, CJK punctuation,
/// fullwidth forms, CJK extensions). Single source of truth shared by the
/// token estimator and the retrieval tokenizer.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F          // CJK punctuation
        | 0x3040..=0x30FF        // Hiragana + Katakana
        | 0x3130..=0x318F        // Hangul compatibility jamo
        | 0x3400..=0x4DBF        // CJK ext A
        | 0x4E00..=0x9FFF        // CJK unified
        | 0xAC00..=0xD7AF        // Hangul syllables
        | 0xF900..=0xFAFF        // CJK compatibility ideographs
        | 0xFF00..=0xFFEF        // fullwidth forms
        | 0x20000..=0x2FA1F      // CJK ext B..F
    )
}

/// Rough token estimate used for chunking and budgeting.
///
/// CJK characters count as one token each; other characters count as one
/// token per four characters, rounded up. Deterministic and allocation-free.
pub fn estimate_tokens(text: &str) -> u32 {
    let mut cjk: u32 = 0;
    let mut other: u32 = 0;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_round_trips_as_scalar() {
        let value = SchemaVersion::CURRENT;
        let encoded = serde_json::to_string(&value).expect("encode schema version");
        let decoded: SchemaVersion = serde_json::from_str(&encoded).expect("decode schema version");
        assert_eq!(value, decoded);
    }

    #[test]
    fn workspace_status_uses_stable_names() {
        let encoded =
            serde_json::to_string(&WorkspaceStatus::Uninitialized).expect("encode status");
        assert_eq!(encoded, r#""uninitialized""#);
    }

    #[test]
    fn source_kind_uses_stable_names() {
        assert_eq!(
            serde_json::to_string(&SourceKind::Note).unwrap(),
            r#""note""#
        );
        assert_eq!(
            serde_json::to_string(&OriginClass::Owner).unwrap(),
            r#""owner""#
        );
    }

    #[test]
    fn estimate_tokens_counts_cjk_per_char() {
        assert_eq!(estimate_tokens("傅里叶变换"), 5);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("。、"), 2);
    }

    #[test]
    fn estimate_tokens_counts_latin_per_four_chars() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("hello world"), 3);
    }

    #[test]
    fn estimate_tokens_handles_mixed_text() {
        // 3 CJK chars (3) + " bert" (5 latin chars -> 2) = 5.
        assert_eq!(estimate_tokens("谱定理 bert"), 5);
    }

    #[test]
    fn index_policy_has_documented_default() {
        let policy = IndexPolicy::default();
        assert_eq!(policy.lexical, "tantivy_bm25");
        assert!(!policy.hybrid);
        assert_eq!(policy.cjk_tokenizer, "cjk");
        assert_eq!(policy.max_chunk_tokens, 512);
        assert_eq!(policy.chunk_overlap_tokens, 64);
    }

    #[test]
    fn retrieval_hit_round_trips() {
        let hit = RetrievalHit {
            rank: 1,
            source_key: "ml/fourier.md".to_string(),
            revision_hash: "abc".to_string(),
            chunk_id: "cid".to_string(),
            chunk_hash: "chash".to_string(),
            ordinal: 3,
            heading_path: Some("Fourier > 性质".to_string()),
            score: 0.87,
            locator_json: "{}".to_string(),
        };
        let encoded = serde_json::to_string(&hit).unwrap();
        let decoded: RetrievalHit = serde_json::from_str(&encoded).unwrap();
        assert_eq!(hit, decoded);
    }
}
