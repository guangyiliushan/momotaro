//! Versioned contracts shared by the Momotaro runtime surfaces.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// The schema version understood by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

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

/// Scheme prefix for vault notes: `note:<vault-relative NFC path>`.
pub const NOTE_SCHEME: &str = "note:";

/// Failure raised while building a source key from a vault-relative tail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceKeyError {
    /// The tail was empty after normalization.
    #[error("source key tail is empty")]
    Empty,
    /// The tail was an absolute path (POSIX, UNC, or drive-absolute form).
    #[error("source key tail must be vault-relative, got an absolute form: {0:?}")]
    Absolute(String),
    /// The tail contained a `..` component.
    #[error("source key tail must not contain `..`")]
    ParentDir,
    /// The tail contained a NUL byte.
    #[error("source key tail contains a NUL byte")]
    Nul,
}

/// Builds `note:<vault-relative NFC path>`.
///
/// The input MUST already be a `/`-joined sequence of path components — that
/// is what the vault walker hands over. Bytes other than `/` are name bytes
/// and are never rewritten: a backslash inside a name stays a backslash, so
/// one file literally named `docs\ml\a.md` and the nested path `docs/ml/a.md`
/// remain two identities.
///
/// Key rules (ADR 0002 / D42): the tail is NFC-normalized; case is preserved
/// verbatim; `/` is the only separator; `.` components are dropped; absolute
/// and `..`-bearing tails are rejected. A key is an identity, never a
/// filesystem path. NFKC and case folding are forbidden — folding two distinct
/// Linux files into one identity is a data-loss class accident. No byte of a
/// name is ever rewritten: characters illegal on Windows (`:`, `\`, …) are
/// kept verbatim, because the vault layer indexes such names and warns (D42)
/// while the key layer must keep two distinct files distinct.
pub fn note_source_key(vault_relative: &str) -> Result<String, SourceKeyError> {
    let tail = normalize_key_tail(vault_relative)?;
    Ok(format!("{NOTE_SCHEME}{tail}"))
}

fn normalize_key_tail(raw: &str) -> Result<String, SourceKeyError> {
    if raw.contains('\0') {
        return Err(SourceKeyError::Nul);
    }
    if is_absolute_tail(raw) {
        return Err(SourceKeyError::Absolute(raw.to_owned()));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(SourceKeyError::ParentDir),
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(SourceKeyError::Empty);
    }
    let tail = parts.join("/");
    Ok(tail.nfc().collect())
}

/// POSIX-absolute (`/x`), UNC (`\\server\share`) or drive-absolute
/// (`C:/x`, `C:\x`).
///
/// Backslashes that are not part of such a prefix stay ordinary name
/// characters: a backslash is legal in a POSIX filename, and rewriting it
/// would fold two distinct files — `docs\ml\a.md` and the nested path
/// `docs/ml/a.md` — into one identity. A drive-relative tail (`C:a.md`) is
/// likewise kept verbatim: the vault walker never produces one, and guessing
/// would be a rename.
fn is_absolute_tail(raw: &str) -> bool {
    if raw.starts_with('/') || raw.starts_with(r"\\") {
        return true;
    }
    let bytes = raw.as_bytes();
    bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
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
    /// Facts about the SQLite engine this build links.
    #[serde(default)]
    pub sqlite: Option<SqliteFacts>,
    /// User-facing explanation for non-ready states.
    pub message: Option<String>,
}

/// Facts about the SQLite engine this build links, surfaced by `doctor`.
///
/// Read through SQL on a scratch connection — never ffi or `unsafe` — so the
/// engine version can be reported even when the workspace database is missing
/// or unreadable, which is exactly when it matters most.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteFacts {
    /// `SELECT sqlite_version()`, e.g. `3.53.2`.
    pub version: String,
    /// `sqlite3_libversion_number()`, e.g. `3053002`.
    pub version_number: i32,
    /// `SELECT sqlite_source_id()` — the amalgamation this engine came from.
    pub source_id: String,
    /// Every row of `PRAGMA compile_options`.
    pub compile_options: Vec<String>,
    /// True when the engine sits inside the WAL-reset corruption window.
    pub wal_reset_window: bool,
    /// True for the withdrawn 3.52 series — refused for a different reason.
    pub withdrawn_release: bool,
}

/// Whether a `sqlite3_libversion_number()` value sits in the WAL-reset window.
///
/// Upstream wording (sqlite.org/wal.html §11): the bug is "likely present in all
/// versions of SQLite from 3.7.0 (2010-07-21) through 3.51.2 (2026-01-09)", and
/// "fixed in version 3.51.3 (2026-03-13) and later". Two releases inside the
/// window were patched in place: 3.44.6 and 3.50.7.
pub fn is_wal_reset_window(version_number: i32) -> bool {
    /// 3.7.0 — where WAL reset landed.
    const FIRST_AFFECTED: i32 = 3_007_000;
    /// 3.51.3 — the first release upstream calls fixed (exclusive bound).
    const FIRST_FIXED: i32 = 3_051_003;
    /// 3.44.6 and 3.50.7: patched inside the window.
    const BACKPORTED: [i32; 2] = [3_044_006, 3_050_007];

    (FIRST_AFFECTED..FIRST_FIXED).contains(&version_number) && !BACKPORTED.contains(&version_number)
}

/// Whether the engine is a release of the withdrawn 3.52 series.
///
/// A different refusal from the corruption window: 3.52.0 was withdrawn for
/// misreporting `integrity_check` corruption, so it must never be recommended
/// as the way out of the window either.
pub fn is_withdrawn_release(version_number: i32) -> bool {
    (3_052_000..3_053_000).contains(&version_number)
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

impl SourceKind {
    /// The wire spelling, as stored in SQLite.
    ///
    /// One source of truth for the column value; `serde`'s `snake_case` renaming
    /// must keep agreeing with it, and `the_kind_spelling_matches_serde` pins both.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Paper => "paper",
            Self::Web => "web",
        }
    }
}

/// Failure to parse a [`SourceKind`] from its wire spelling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown source kind: {0}")]
pub struct UnknownSourceKind(String);

impl std::str::FromStr for SourceKind {
    type Err = UnknownSourceKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "note" => Ok(Self::Note),
            "paper" => Ok(Self::Paper),
            "web" => Ok(Self::Web),
            other => Err(UnknownSourceKind(other.to_owned())),
        }
    }
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

impl OriginClass {
    /// The wire spelling of this class (`owner`, `paper`, …).
    ///
    /// One source of truth for the SQLite column, the index field and the JSON
    /// form — `serde`'s `snake_case` renaming must keep agreeing with it, and
    /// `the_spelling_matches_serde` pins that.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Paper => "paper",
            Self::Web => "web",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

/// Failure to parse an [`OriginClass`] from its wire spelling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown origin class: {0}")]
pub struct UnknownOriginClass(String);

impl std::str::FromStr for OriginClass {
    type Err = UnknownOriginClass;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "owner" => Ok(Self::Owner),
            "paper" => Ok(Self::Paper),
            "web" => Ok(Self::Web),
            "agent" => Ok(Self::Agent),
            "system" => Ok(Self::System),
            other => Err(UnknownOriginClass(other.to_owned())),
        }
    }
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
    /// Workspace-relative, `/`-joined path when the source lives on disk
    /// (ADR 0024) — never absolute.
    pub local_path: Option<String>,
    /// The file name exactly as the vault spelled it, kept for rename matching,
    /// collision reporting and display. Never an identity: the key is.
    pub raw_name: Option<String>,
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
    /// Relevance score: the engine's BM25 multiplied by the trust class's
    /// weight (`momotaro_retrieve::boost`), so scores of documents in different
    /// classes are not directly comparable magnitudes.
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

/// Why one file failed during an index run.
///
/// Callers branch on this, never on `message` text: the message is for humans
/// and is free to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexFileErrorKind {
    /// Two distinct files in one batch derived the same source key.
    KeyCollision,
    /// The file name is not valid UTF-8.
    InvalidUtf8,
    /// The path is not a name inside the vault root. Covers the wider
    /// workspace case too: since ADR 0024 every stored path is
    /// workspace-relative, so a path outside the workspace that contains the
    /// vault cannot be described either.
    OutsideVault,
    /// The derived source key violated the key contract.
    InvalidSourceKey,
    /// Anything else: IO, store, chunking.
    Other,
}

impl std::fmt::Display for IndexFileErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::KeyCollision => "key_collision",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::OutsideVault => "outside_vault",
            Self::InvalidSourceKey => "invalid_source_key",
            Self::Other => "other",
        };
        formatter.write_str(text)
    }
}

/// Per-file failure recorded during an index run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexFileError {
    /// Source key of the file that failed, when one could be derived.
    pub source_key: Option<String>,
    /// Machine-readable failure kind.
    pub kind: IndexFileErrorKind,
    /// Human-readable detail; never parse this.
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
    fn the_spelling_matches_serde() {
        // The SQLite column, the index field and the JSON form must never
        // disagree about what a class is called.
        for class in [
            OriginClass::Owner,
            OriginClass::Paper,
            OriginClass::Web,
            OriginClass::Agent,
            OriginClass::System,
        ] {
            let json = serde_json::to_value(class).expect("serialize class");
            assert_eq!(
                class.as_str(),
                json.as_str().expect("class is a string"),
                "as_str and serde disagree for {class:?}"
            );
            assert_eq!(class.as_str().parse::<OriginClass>(), Ok(class));
        }
        assert!("nope".parse::<OriginClass>().is_err());
    }

    #[test]
    fn wal_reset_window_boundaries() {
        // Upstream wording: present from 3.7.0 through 3.51.2, fixed in 3.51.3;
        // the backports 3.44.6 and 3.50.7 count as fixed.
        assert!(!is_wal_reset_window(3_006_999));
        assert!(is_wal_reset_window(3_007_000));
        assert!(is_wal_reset_window(3_050_002));
        assert!(
            is_wal_reset_window(3_051_002),
            "3.51.2 is the last affected"
        );
        assert!(!is_wal_reset_window(3_044_006));
        assert!(!is_wal_reset_window(3_050_007));
        assert!(
            !is_wal_reset_window(3_051_003),
            "3.51.3 is the first fixed release upstream names"
        );
        assert!(
            !is_wal_reset_window(3_052_000),
            "3.52.x is withdrawn, not inside the window"
        );
        assert!(!is_wal_reset_window(3_053_000));
        assert!(!is_wal_reset_window(3_053_002));
    }

    #[test]
    fn the_withdrawn_3_52_series_is_refused_on_its_own_terms() {
        assert!(is_withdrawn_release(3_052_000));
        assert!(is_withdrawn_release(3_052_099));
        assert!(!is_withdrawn_release(3_051_003));
        assert!(!is_withdrawn_release(3_053_000));
        assert!(!is_withdrawn_release(3_053_002));
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

    // --- source-key contract (ADR 0002 / D42) ---

    #[test]
    fn the_kind_spelling_matches_serde() {
        // The column value, the JSON form and `FromStr` are one spelling; the
        // store decodes through `FromStr` and writes through `as_str`.
        for kind in [SourceKind::Note, SourceKind::Paper, SourceKind::Web] {
            assert_eq!(kind.as_str().parse::<SourceKind>(), Ok(kind));
            assert_eq!(
                serde_json::to_string(&kind).expect("serialize"),
                format!("\"{}\"", kind.as_str()),
                "serde agrees with as_str"
            );
        }
        assert!("nope".parse::<SourceKind>().is_err());
    }

    #[test]
    fn note_key_is_scheme_prefixed() {
        assert_eq!(
            note_source_key("ml/fourier.md").expect("valid tail"),
            "note:ml/fourier.md"
        );
    }

    #[test]
    fn note_key_preserves_case_verbatim() {
        assert_eq!(note_source_key("A.md").unwrap(), "note:A.md");
        assert_ne!(
            note_source_key("A.md").unwrap(),
            note_source_key("a.md").unwrap(),
            "the key layer must never fold case"
        );
    }

    #[test]
    fn note_key_is_nfc_normalized() {
        // "é" written as e + U+0301 must land on the same identity as U+00E9.
        assert_eq!(
            note_source_key("cafe\u{0301}.md").unwrap(),
            note_source_key("caf\u{00e9}.md").unwrap()
        );
        assert_eq!(
            note_source_key("caf\u{00e9}.md").unwrap(),
            "note:caf\u{00e9}.md"
        );
    }

    #[test]
    fn note_key_rejects_empty_and_parent_dir() {
        assert!(matches!(note_source_key(""), Err(SourceKeyError::Empty)));
        assert!(matches!(note_source_key("./"), Err(SourceKeyError::Empty)));
        assert!(matches!(
            note_source_key("ml/../secret.md"),
            Err(SourceKeyError::ParentDir)
        ));
    }

    #[test]
    fn note_key_rejects_absolute_forms() {
        for bad in [
            "/etc/passwd",
            r"C:\ws\a.md",
            "C:/ws/a.md",
            r"\\server\share\a.md",
        ] {
            assert!(
                matches!(note_source_key(bad), Err(SourceKeyError::Absolute(_))),
                "{bad:?} must be rejected as an absolute form"
            );
        }
    }

    #[test]
    fn note_key_rejects_nul() {
        assert!(matches!(
            note_source_key("a\0b.md"),
            Err(SourceKeyError::Nul)
        ));
    }

    #[test]
    fn index_error_kind_display_matches_serde() {
        // The terminal reads Display, the JSON reads serde; if they drift,
        // "branch on the kind" silently means two different things.
        for kind in [
            IndexFileErrorKind::KeyCollision,
            IndexFileErrorKind::InvalidUtf8,
            IndexFileErrorKind::OutsideVault,
            IndexFileErrorKind::InvalidSourceKey,
            IndexFileErrorKind::Other,
        ] {
            let json = serde_json::to_value(kind).expect("serialize kind");
            assert_eq!(
                kind.to_string(),
                json.as_str().expect("kind serializes to a string"),
                "Display and serde must agree for {kind:?}"
            );
        }
    }

    #[test]
    fn note_key_drops_dot_components() {
        assert_eq!(note_source_key("./a/./b.md").unwrap(), "note:a/b.md");
    }

    #[test]
    fn note_key_rejects_parent_dir_in_every_position() {
        for bad in ["..", "../x.md", "a/..", "a/../b.md", "a/b/../.."] {
            assert!(
                matches!(note_source_key(bad), Err(SourceKeyError::ParentDir)),
                "{bad:?} must be rejected"
            );
        }
    }

    /// A backslash is a legal filename character on POSIX. Rewriting it to `/`
    /// would be a silent rename and would fold two distinct vault files into
    /// one identity (independent-review finding, 2026-09-16).
    #[test]
    fn note_key_keeps_backslashes_verbatim() {
        assert_eq!(note_source_key(r"a\b.md").unwrap(), r"note:a\b.md");
    }

    #[test]
    fn note_key_keeps_backslash_names_distinct_from_nested_paths() {
        assert_ne!(
            note_source_key(r"docs\ml\a.md").unwrap(),
            note_source_key("docs/ml/a.md").unwrap(),
            "one file named `docs\\ml\\a.md` is not the nested path docs/ml/a.md"
        );
    }

    /// A single leading backslash is Windows root-relative but a legal POSIX
    /// name; like other Windows-illegal names it is kept verbatim (D42 indexes
    /// such names and warns at the vault layer).
    #[test]
    fn note_key_keeps_leading_backslash_name_verbatim() {
        assert_eq!(
            note_source_key(r"\etc\passwd").unwrap(),
            r"note:\etc\passwd"
        );
    }

    #[test]
    fn note_key_keeps_windows_relative_colon_forms_verbatim() {
        // Drive-relative tails never come out of the vault walker; if one is
        // passed in it stays a name rather than being guessed at.
        assert_eq!(note_source_key("C:a.md").unwrap(), "note:C:a.md");
    }

    #[test]
    fn note_key_rejects_lowercase_drive_absolute_forms() {
        assert!(matches!(
            note_source_key("c:/ws/a.md"),
            Err(SourceKeyError::Absolute(_))
        ));
        assert!(matches!(
            note_source_key(r"c:\ws\a.md"),
            Err(SourceKeyError::Absolute(_))
        ));
    }

    /// Equivalence classes the contract blesses: dot components, empty
    /// components and a trailing separator all collapse, and NFC-equivalent
    /// spellings share one identity. Nothing else is folded.
    #[test]
    fn note_key_collapses_redundant_separators() {
        assert_eq!(note_source_key("a//b.md").unwrap(), "note:a/b.md");
        assert_eq!(note_source_key("a.md/").unwrap(), "note:a.md");
        assert_eq!(note_source_key("a.md").unwrap(), "note:a.md");
    }

    #[test]
    fn note_key_keeps_windows_illegal_characters_verbatim() {
        // `:` is legal on POSIX; D42 indexes such names and warns, so the key
        // layer must not rewrite or reject them.
        assert_eq!(note_source_key("a:b.md").unwrap(), "note:a:b.md");
    }
}
