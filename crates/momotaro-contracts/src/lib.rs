//! Versioned contracts shared by the Momotaro runtime surfaces.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// The schema version understood by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 4;

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
    /// A vault file that is registered rather than indexed: images, audio,
    /// video, PDFs and anything else the whitelist does not read. It is a
    /// source (it has a key and a revision) with no chunks — `docs/docs/
    /// ingest-interop.md` §5 第 10 条 says which files these are, and why the
    /// index never pretends its bytes are searchable.
    Attachment,
}

impl SourceKind {
    /// Every kind, for readers and tests that must not miss a new one.
    pub const ALL: [Self; 4] = [Self::Note, Self::Paper, Self::Web, Self::Attachment];

    /// The wire spelling, as stored in SQLite.
    ///
    /// One source of truth for the column value; `serde`'s `snake_case` renaming
    /// must keep agreeing with it, and `the_kind_spelling_matches_serde` pins both.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Paper => "paper",
            Self::Web => "web",
            Self::Attachment => "attachment",
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
            "attachment" => Ok(Self::Attachment),
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
    /// A string that must be UTF-8 is not: a file name, a path component, or a
    /// file's bytes. This arrived as "not a valid file name"; the index run also
    /// reports it for content, so the narrower wording was wrong.
    InvalidUtf8,
    /// The path is not a name inside the vault root. Covers the wider
    /// workspace case too: since ADR 0024 every stored path is
    /// workspace-relative, so a path outside the workspace that contains the
    /// vault cannot be described either.
    OutsideVault,
    /// The derived source key violated the key contract.
    InvalidSourceKey,
    /// Reading the file failed (IO).
    Io,
    /// Writing or reading the canonical store failed.
    Store,
    /// The search index refused the document.
    Index,
    /// Anything else.
    Other,
}

impl std::fmt::Display for IndexFileErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::KeyCollision => "key_collision",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::OutsideVault => "outside_vault",
            Self::InvalidSourceKey => "invalid_source_key",
            Self::Io => "io",
            Self::Store => "store",
            Self::Index => "index",
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

/// What a file lost on the way into the index (D41).
///
/// These ride on **`indexed`** rows: the file *was* indexed, so the outcome stays
/// `indexed` and this code says what it lost. All ten of D41's are here; the
/// features that raise the last four (canvas, Excalidraw, symlinks, attachments)
/// land with the vault-syntax work, and each one wires its code when it does.
/// Having them typed is what keeps
/// `degradation_message` exhaustive and stops a sentence from claiming to be about
/// the wrong code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Degradation {
    /// The frontmatter block could not be read (D41.5); the body was indexed.
    FrontmatterParseError,
    /// A `[[wikilink]]` points at a note that is not in the vault (D41.1).
    UnresolvedWikilink,
    /// A `[[wikilink]]` matches more than one note, so no edge is built (D41.2).
    AmbiguousWikilink,
    /// A `#heading` anchor does not exist in the target (D41.3); the edge is kept.
    StaleAnchor,
    /// A markdown link's target could not be percent-decoded (D41.4).
    MarkdownLinkDecodeError,
    /// A private link syntax this build does not know (D41.6), e.g. Dendron's.
    UnknownLinkSyntax,
    /// A `.canvas` block that is not readable as JSON (D41.8); the file keeps its
    /// bytes and still gets indexed.
    CanvasParseError,
    /// A note that is an Excalidraw drawing (D41.7): the compressed scene stays
    /// out of the index while the note's other sections are indexed normally.
    ExcalidrawDetected,
    /// A symlink under the vault (D41.9). The first version does not follow them,
    /// so the link is recorded rather than read.
    SymlinkEncountered,
    /// A file that is registered rather than indexed (D41.10): images, audio,
    /// video and PDFs get a revision with no chunks, so the vault's coverage is
    /// answerable even though their bytes are not searchable.
    AttachmentUnindexed,
}

impl Degradation {
    /// Every code, in the order a file's degradations are reported and counted.
    ///
    /// One list, iterated by `from_code` and by the tests. `the_variant_list_is_
    /// complete` matches every variant without a wildcard, so a code cannot be
    /// added to `as_str` alone — which was the hole: the writer knew a
    /// code the reader's array did not, and the row's code was dropped silently.
    pub const ALL: [Self; 10] = [
        Self::FrontmatterParseError,
        Self::UnresolvedWikilink,
        Self::AmbiguousWikilink,
        Self::StaleAnchor,
        Self::MarkdownLinkDecodeError,
        Self::UnknownLinkSyntax,
        Self::CanvasParseError,
        Self::ExcalidrawDetected,
        Self::SymlinkEncountered,
        Self::AttachmentUnindexed,
    ];
    /// The wire spelling written to `store_events.reason_code`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FrontmatterParseError => "frontmatter_parse_error",
            Self::UnresolvedWikilink => "unresolved_wikilink",
            Self::AmbiguousWikilink => "ambiguous_wikilink",
            Self::StaleAnchor => "stale_anchor",
            Self::MarkdownLinkDecodeError => "markdown_link_decode_error",
            Self::UnknownLinkSyntax => "unknown_link_syntax",
            Self::CanvasParseError => "canvas_parse_error",
            Self::ExcalidrawDetected => "excalidraw_detected",
            Self::SymlinkEncountered => "symlink_encountered",
            Self::AttachmentUnindexed => "attachment_unindexed",
        }
    }

    /// The code back as a variant, for the readers of stored rows.
    ///
    /// `None` is a row written by a version that knows a code this one does not;
    /// the caller decides whether that is worth saying, and [`Self::as_str`] is the
    /// only writer, so the two cannot drift apart silently.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == code)
    }
}

/// What will not survive another platform, though nothing was lost here (D42).
///
/// A warning is not a [`Degradation`]: the ten degradations say what a file
/// lost on the way into the index, while a warning is a standing fact about
/// the file — a cloud placeholder whose bytes are not on this device, an iCloud
/// eviction stub in a note's place, a name Windows' own naming rules reserve.
/// Nothing was lost by the file, the ten stay ten, and a reader that
/// counts degradations is not taught a fourth kind of loss. The two
/// vocabularies share the `reason_code` column and a row's `data_json`; a
/// reader must consult both before calling a code unknown (ADR 0026, "A
/// warning is not a degradation").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warning {
    /// A cloud placeholder (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`): its bytes
    /// are not on this device, so it was not read (D42).
    ///
    /// The explicit rename is load-bearing: `snake_case` alone would split the
    /// product name into `one_drive_placeholder`, and the witness test pins the
    /// two spellings together.
    #[serde(rename = "onedrive_placeholder")]
    OneDrivePlaceholder,
    /// An iCloud eviction stub (`.name.icloud`): the note it stands for is not
    /// here; the stub itself is skipped and never read (D42).
    IcloudPlaceholder,
    /// A name Windows' own naming rules reserve (reserved device name, reserved
    /// character, trailing dot or space): indexed here, refused or mangled by
    /// the tools and sync clients that follow the rules (D42).
    WindowsIncompatibleName,
}

impl Warning {
    /// Every warning, in the order a row's warnings are reported and counted
    /// — the declaration order, which is what `Ord` gives the row writer's
    /// sort and the report's map.
    ///
    /// One list, iterated by `from_code` and by the tests:
    /// `the_warning_list_is_complete` matches every variant without a
    /// wildcard, so a code cannot be added to `as_str` alone, and
    /// `the_warning_list_is_in_the_order_rows_are_counted` keeps this array
    /// and the declaration order agreeing.
    pub const ALL: [Self; 3] = [
        Self::OneDrivePlaceholder,
        Self::IcloudPlaceholder,
        Self::WindowsIncompatibleName,
    ];

    /// The wire spelling written to `store_events.reason_code` and to the
    /// row's `warnings` payload.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneDrivePlaceholder => "onedrive_placeholder",
            Self::IcloudPlaceholder => "icloud_placeholder",
            Self::WindowsIncompatibleName => "windows_incompatible_name",
        }
    }

    /// The code back as a variant, for the readers of stored rows.
    ///
    /// `None` is a code from a vocabulary this build does not know (or one from
    /// the other vocabulary); the caller decides, and [`Self::as_str`] is the
    /// only writer, so the two cannot drift apart silently.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == code)
    }
}

/// How the rows carrying one code were handled.
///
/// Two axes, because that is what the log holds: an `indexed`/`metadata_refreshed`
/// row is in the index anyway, a `skipped` row was not read (a link is not
/// followed; an unchanged attachment is not read again; an eviction stub or a
/// cloud placeholder is not read at all), and both can carry the same code —
/// whether it is a [`Degradation`] or a [`Warning`], which is why this type
/// carries no vocabulary of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageCounts {
    /// Rows that went into the index anyway.
    pub in_index: u64,
    /// Rows nothing was read for.
    pub skipped: u64,
}

impl CoverageCounts {
    /// Both axes, for a reader that only wants the total.
    #[must_use]
    pub fn total(self) -> u64 {
        self.in_index + self.skipped
    }
}

/// What the most recent ingest batch did, read back out of `store_events`
/// (ADR 0026). `None` comes back from `momotaro_run::coverage_report` when no
/// batch was ever recorded — never because one failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageReport {
    /// The batch these counts describe.
    pub batch_id: String,
    /// Rows that wrote a revision and its chunks.
    pub indexed: u64,
    /// Rows nothing was read for: the file was unchanged, a link was not
    /// followed, or a stub or placeholder was not read. The row's code says
    /// which.
    pub skipped: u64,
    /// Rows whose file only moved or was re-spelled.
    pub refreshed: u64,
    /// What the rows carrying a code lost on the way in, counted per code **and per
    /// row state** (D41).
    ///
    /// The state is the row's, never the code's: the same code rides rows that are in
    /// the index anyway and rows nothing was read for (`symlink_encountered` on a
    /// `skipped` link, `attachment_unindexed` on an attachment that was not read
    /// again), and a summary that guessed from the code contradicted its own counts.
    /// Without this, a note whose block could not be read is indistinguishable from a
    /// clean one in this report: the row says `indexed` and the code was only in the
    /// event table.
    pub degradations: BTreeMap<Degradation, CoverageCounts>,
    /// What the rows carrying a warning said (D42), counted like [`Self::degradations`].
    ///
    /// A warning is not a loss: the file is in the index, or was skipped, and the
    /// code says what will not survive another platform — a name no ordinary
    /// Windows machine accepts, a placeholder whose bytes are not here, an
    /// eviction stub where a note used to be. Same two axes, same rule: the
    /// state is the row's, never the code's.
    pub warnings: BTreeMap<Warning, CoverageCounts>,
    /// Rows that failed, in the batch's own order (`seq`, not `id`).
    pub failures: Vec<CoverageFailure>,
    /// Every row in the batch, including its beginning and its end.
    pub events: u64,
    /// Whether the batch reached `ingest.finished`.
    pub finished: bool,
}

/// One file a batch could not ingest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageFailure {
    /// The source the row is about, when it has one.
    pub source_key: Option<String>,
    /// The stable code (`key_collision`, `outside_vault`, …).
    pub reason_code: Option<String>,
    /// The detail the row carried.
    pub message: Option<String>,
}

/// Aggregate outcome of one `momotaro index` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexReport {
    /// Identifier of this indexing batch, as written to `store_events`
    /// (ADR 0026): one `index` run, one batch.
    pub batch_id: String,
    /// Files the vault walk kept — notes, canvases, attachments, links and
    /// eviction stubs alike, since all of them get rows (§5 第 10 条).
    pub files_scanned: u64,
    /// Files nothing was read for: the fingerprint was unchanged, the walk saw a
    /// link it does not follow, or the file is an eviction stub or a cloud
    /// placeholder. All are skips, and the row's code says which.
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
    fn the_variant_list_is_complete() {
        // No wildcard in this match: adding a seventh variant fails to compile
        // here, and the count assertion then forces it into `ALL`.
        for code in Degradation::ALL {
            match code {
                Degradation::FrontmatterParseError => {}
                Degradation::UnresolvedWikilink => {}
                Degradation::AmbiguousWikilink => {}
                Degradation::StaleAnchor => {}
                Degradation::MarkdownLinkDecodeError => {}
                Degradation::UnknownLinkSyntax => {}
                Degradation::CanvasParseError => {}
                Degradation::ExcalidrawDetected => {}
                Degradation::SymlinkEncountered => {}
                Degradation::AttachmentUnindexed => {}
            }
        }
        assert_eq!(Degradation::ALL.len(), 10, "extend ALL, not just as_str");
    }

    #[test]
    fn a_degradation_code_parses_back_to_its_variant() {
        // `from_code` is how a reader turns a stored row back into a code; the
        // spelling is the only contract, so it is the only thing to check.
        for code in Degradation::ALL {
            assert_eq!(Degradation::from_code(code.as_str()), Some(code));
        }
        assert_eq!(
            Degradation::from_code("a_code_from_the_future"),
            None,
            "an unknown code is none of our business, not a panic"
        );
    }

    #[test]
    fn every_degradation_spells_the_same_on_the_wire() {
        // Same rule as the failure kinds above, same caveat: the list is written by
        // hand, so a seventh code means adding it here as well.
        for code in Degradation::ALL {
            assert_eq!(
                serde_json::to_string(&code).expect("serialize"),
                format!("\"{}\"", code.as_str()),
                "{code:?} spells differently in JSON and in Display (as_str)"
            );
        }
    }

    #[test]
    fn the_warning_list_is_complete() {
        // Same rule as the degradation list above: no wildcard here, so a fourth
        // warning fails to compile, and the count then forces it into `ALL`.
        for warning in Warning::ALL {
            match warning {
                Warning::OneDrivePlaceholder => {}
                Warning::IcloudPlaceholder => {}
                Warning::WindowsIncompatibleName => {}
            }
        }
        assert_eq!(Warning::ALL.len(), 3, "extend ALL, not just as_str");
    }

    #[test]
    fn a_warning_code_parses_back_to_its_variant() {
        for warning in Warning::ALL {
            assert_eq!(Warning::from_code(warning.as_str()), Some(warning));
        }
        assert_eq!(
            Warning::from_code("a_code_from_the_future"),
            None,
            "an unknown code is none of our business, not a panic"
        );
        assert_eq!(
            Warning::from_code("frontmatter_parse_error"),
            None,
            "the vocabularies are separate: a degradation is not a warning"
        );
    }

    #[test]
    fn every_warning_spells_the_same_on_the_wire() {
        for warning in Warning::ALL {
            assert_eq!(
                serde_json::to_string(&warning).expect("serialize"),
                format!("\"{}\"", warning.as_str()),
                "{warning:?} spells differently in JSON and in Display (as_str)"
            );
        }
    }

    #[test]
    fn the_warning_list_is_in_the_order_rows_are_counted() {
        // `ALL` claims to be "the order a row's warnings are reported and
        // counted", and the row writer's `sort_unstable` and the report's
        // `BTreeMap` both read the *declaration* order through `Ord`. Reorder
        // one side alone and a two-warning row's primary code and payload list
        // flip silently — `a|b.md.icloud` is exactly such a row.
        let mut sorted = Warning::ALL;
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            Warning::ALL,
            "reorder the enum and `ALL` together, or a row's order changes"
        );
    }

    #[test]
    fn every_failure_kind_spells_the_same_on_the_wire() {
        // The terminal reads Display, the JSON reads serde; if they drift, "branch
        // on the kind" silently means two different things. The list below is
        // hand-written: a review measured that a ninth variant IS NOT forced into
        // this loop, so adding one means adding it here too.
        for kind in [
            IndexFileErrorKind::KeyCollision,
            IndexFileErrorKind::InvalidUtf8,
            IndexFileErrorKind::OutsideVault,
            IndexFileErrorKind::InvalidSourceKey,
            IndexFileErrorKind::Io,
            IndexFileErrorKind::Store,
            IndexFileErrorKind::Index,
            IndexFileErrorKind::Other,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).expect("serialize"),
                format!("\"{kind}\""),
                "{kind:?} spells differently in JSON and in Display"
            );
        }
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
    fn every_source_kind_is_in_all() {
        // A no-wildcard match, the same guard `Degradation` has: a new kind cannot be
        // added to `as_str` alone, and the list cannot quietly lose one either.
        for kind in SourceKind::ALL {
            match kind {
                SourceKind::Note | SourceKind::Paper | SourceKind::Web | SourceKind::Attachment => {
                }
            }
        }
        assert_eq!(SourceKind::ALL.len(), 4, "extend ALL, not just as_str");
    }

    #[test]
    fn the_kind_spelling_matches_serde() {
        // The column value, the JSON form and `FromStr` are one spelling; the
        // store decodes through `FromStr` and writes through `as_str`.
        // Iterates `ALL`, so a new kind cannot be added to `as_str` alone: the same
        // guard `Degradation` has, for the same hole.
        for kind in SourceKind::ALL {
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
