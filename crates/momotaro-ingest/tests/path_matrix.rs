//! The platform/path test matrix (key rows implement the key scheme,
//! path rows implement workspace containment).
//!
//! One named inventory for the platform matrix so both CI legs run exactly
//! this list. Windows-only cases compile out on the ubuntu leg (`#[cfg]`), so a
//! green ubuntu run means "the portable half passed", never "the Windows half
//! was checked". The table below is the single source of the 18-row count;
//! rows deliberately restate assertions unit tests also pin — a named
//! inventory is the deliverable, and each row must hold up when this target
//! runs alone.
//!
//! # The 18-vs-11 count, reconciled once (it is two counts of one surface)
//!
//! - **11 项** counts *semantic rows* of the
//!   key-layer/sync-disk matrix, splitting on `/` with the parenthesis
//!   protected: 大小写双写 · 仅大小写改名 · **NFD** · **NFC+NFD 双写** ·
//!   260 长路径 · 保留名与尾随字符 · OneDrive placeholder · iCloud placeholder ·
//!   三种路径表述同一键 · 折叠陷阱字符 · exFAT 移动盘.
//! - **18 例** counts *test cases* in this file: the same
//!   surface with the NFD pair merged into one case (one fixture, two
//!   assertions) — 10 cases — plus 8 path-resolution/security cases the key-layer
//!   matrix does not count (they come from the vault ⊆ workspace contract):
//!   junction · `..` 注入 · 绝对注入 POSIX · 绝对注入 盘符 · 绝对注入 UNC ·
//!   跨卷 · 8.3 短名 · 尚未创建的路径.  11 − 1 + 8 = 18.
//!
//! | card 例 | 项 | test(s) in this file | tier | kills (mutation / falsifier) |
//! |---|---|---|---|---|
//! | junction | — | `junction_is_recorded_and_never_an_escape` | both legs (J on win, symlink on unix) | follow the link ⇒ extra walk entry; drop containment re-check ⇒ resolve escapes; a lexical `is_within` ⇒ the `!is_within` pin red |
//! | `..` 注入 | — | `parent_dir_injection_is_rejected_everywhere` | both legs | strip-and-revalidate instead of reject ⇒ any acceptance is red |
//! | 绝对注入 POSIX | — | `posix_absolute_forms_are_rejected` | both legs | drop the leading-`/` key guard ⇒ the loop red both legs; drop the resolve form arm ⇒ the in-vault pins red (host-absolute spelling on either leg, root-relative spelling on windows) — the outside-the-workspace tails alone would mask it |
//! | 绝对注入 盘符 | — | `drive_absolute_forms_are_rejected` | both legs (resolve half cfg-split) | drop the drive-form key guard ⇒ the loop red both legs; drop the resolve form arm ⇒ the in-vault pins red; the `C:a.md` pin flips if the guard is widened |
//! | 绝对注入 UNC | — | `unc_absolute_forms_are_rejected` | both legs (resolve half cfg-split) | drop the `\\` key guard ⇒ the loop red both legs; drop the resolve form arm ⇒ the in-vault verbatim pin red; folding `\`-leading names into the UNC form flips the `\a.md` pin (drive row) |
//! | 大小写双写 | 1 | `case_double_write_stays_two_identities` | pure both; FS fixture cfg-split (twins only on a case-sensitive volume) | any casefold in the key merges the pair ⇒ red |
//! | 仅大小写改名 | 2 | `case_only_rename_is_a_new_identity` | both legs | casefold ⇒ the two keys compare equal; raw_name losing case ⇒ the rename half red |
//! | NFD/NFC 双写 | 3+4 | `nfd_names_key_as_nfc_and_twins_collide` | both legs (NTFS does not normalize) | drop NFC ⇒ twin keys differ; drop one twin from the walk ⇒ count red |
//! | 260 长路径 | 5 | `paths_past_260_key_exactly` | both legs (win via `\\?\`) | truncation / length-based folding ⇒ key mismatch; the length witness kills a too-short fixture |
//! | 保留名与尾随空格点 | 6 | `reserved_names_warn_and_keep_their_bytes` | pure both; FS fixture per platform | fold or drop a reserved name ⇒ set compare red; widened predicate ⇒ legal names flagged; an NFKC/case fold in the key flips the `com¹.md` byte pin |
//! | OneDrive placeholder | 7 | `placeholder_criterion_is_one_bit_and_ordinary_files_lack_it` + `a_real_onedrive_placeholder_is_recognized_and_plain_to_the_walk` | auto both + **manual once** (real sample) | widening the criterion to OFFLINE/RECALL_ON_OPEN ⇒ the bit pins red; attributing ordinary files as placeholders ⇒ the attribute chain red; the manual half pins the bit on an actual placeholder |
//! | iCloud placeholder | 8 | `icloud_stubs_are_classified_and_never_indexed` | both legs | drop the stub exception in `classify` ⇒ stub vanishes; a suffix-only `icloud_stub` ⇒ the `note.md.icloud` control red |
//! | 三种路径表述同一键 | 9 | `three_spellings_of_one_path_are_one_key` | both legs | drop `.`/empty-component skipping in the key ⇒ trio splits; the `\`-name pin flips if backslash is ever folded to `/` |
//! | 折叠陷阱字符 | 10 | `fold_traps_stay_distinct_and_the_nfc_pair_collides` | pure both; FS fixture per platform (σ twins and `i`/`I` twins unix-only: NTFS upcase folds them) | any casefold merges a pinned pair ⇒ red; dropping NFC splits the U+8005/U+FA5B pair ⇒ red |
//! | exFAT 移动盘 | 11 | `exfat_vault_keys_stay_byte_exact` | **manual once** (real volume) | NFC drop ⇒ twin keys differ; casefold ⇒ survivor spelling wrong |
//! | 跨卷 | — | `cross_volume_link_is_not_an_escape` | **manual once** (second volume) | containment comparing lexically ⇒ escape accepted |
//! | 8.3 短名 | — | `eight_three_short_names_resolve_to_the_long_identity` | **manual once** (8.3 enabled) | comparing pre-canonicalize ⇒ short spelling leaks into `local_path` |
//! | 尚未创建的路径 | — | `not_yet_created_paths_resolve_through_the_nearest_existing_ancestor` | both legs | dropping the missing-tail re-append ⇒ `local_path_in` errors on the missing file |
//!
//! The card's 18 rows land on 19 named tests: the OneDrive row splits into an
//! automated half and its one manual real-sample half (OneDrive 行只手工一次).
//! Every manual test here is `#[ignore]` and **panics with setup
//! steps when its precondition is missing** (env var unset, or the volume
//! feature absent), so "did not run" can never read as "passed" — and so an
//! unfiltered `cargo test -- --ignored` is red on windows for reasons no change
//! caused (on ubuntu the only ignored test is `perf_smoke`). This is the only
//! recipe; the manual rows point back to it — run one by name, locally:
//!
//! ```text
//! RUST_TEST_THREADS=1 cargo test -p momotaro-ingest --test path_matrix -- \
//!     --ignored --nocapture <test_name>
//! ```
//!
//! CI never runs the manual tier. `rust.yml`'s "Path matrix" step re-runs this
//! target by name on both legs — the workspace run above it already covers the
//! auto tier, so the re-run costs seconds and adds no safety; the step exists
//! so the matrix has one named, toggleable CI home. Keeping `--ignored` out of
//! CI is a standing rule, spelled out in the step's comment.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use momotaro_contracts::note_source_key;
use momotaro_ingest::{
    IngestError, attributes_of, canonical_workspace_root, is_cloud_placeholder, is_within,
    local_path_in, raw_name_for, resolve_local_path, source_key_for, walk_entries,
    windows_incompatible_components,
};

/// A fresh vault root.
fn vault() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp vault")
}

/// Writes `rel` (`/`-joined components) under `root`, creating parents.
///
/// Pushes one component at a time so each fixture name lands as exactly one
/// path component whatever a `PathBuf::push` does with separators in a pushed
/// string (current std normalizes them on Windows — measured — but that is
/// std's behaviour, not this helper's contract).
fn write(root: &Path, rel: &str, contents: &str) {
    let mut full = root.to_path_buf();
    for part in rel.split('/') {
        full.push(part);
    }
    fs::create_dir_all(full.parent().expect("parent")).expect("create dir");
    fs::write(&full, contents).expect("write file");
}

/// The canonical root a caller obtains once per run.
fn canonical(root: &Path) -> PathBuf {
    canonical_workspace_root(root).expect("canonical root")
}

/// Every `source_key` the walk derives, in walk order.
fn keys_of(root: &Path) -> Vec<String> {
    walk_entries(root)
        .expect("walk")
        .into_iter()
        .map(|entry| source_key_for(root, &entry.path).expect("source key"))
        .collect()
}

/// Creates a directory link `root/name` → `target`: a junction on Windows (no
/// elevation), a symlink on unix. A silent skip would make "did not run" look
/// exactly like "passed", so creation failure is a hard failure.
fn make_link(root: &Path, name: &str, target: &Path) {
    #[cfg(windows)]
    {
        // `cmd /C` re-parses its command line and TEMP/user paths can carry
        // spaces and metacharacters: quote both paths verbatim (raw_arg, so
        // std does not re-quote the quotes).
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "mklink", "/J"]);
        cmd.raw_arg(format!("\"{}\"", root.join(name).display()));
        cmd.raw_arg(format!("\"{}\"", target.display()));
        let output = cmd.output().expect("run mklink");
        assert!(
            output.status.success(),
            "this test needs to create a junction: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, root.join(name)).expect("create the symlink");
    }
}

/// `path` in the form that bypasses Win32's 260-character and name parsing:
/// the `\\?\` verbatim prefix. Identity off Windows.
#[cfg(windows)]
fn verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    assert!(
        text.starts_with(r"\\") || (text.len() >= 2 && text.as_bytes()[1] == b':'),
        "expected an absolute tempdir path, got {text:?}"
    );
    PathBuf::from(format!(r"\\?\{text}"))
}

#[cfg(not(windows))]
fn verbatim(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// The disk number of a canonical `\\?\X:\…` path — what "same volume" means
/// for the cross-volume row (a canonical local path always carries it).
#[cfg(windows)]
fn drive_of(canonical_path: &Path) -> u8 {
    match canonical_path.components().next() {
        Some(std::path::Component::Prefix(p)) => match p.kind() {
            std::path::Prefix::VerbatimDisk(d) => d,
            other => panic!("expected a local disk path, got {other:?}"),
        },
        other => panic!("expected a canonical local path, got {other:?}"),
    }
}

/// Rejection is by form, not by where the path lands: the tails in the
/// absolute-form loops land outside the workspace and fail through the
/// containment re-check even with a form guard missing. This asserts the
/// non-relative spellings of a file that IS inside the workspace are refused —
/// the in-vault pins that actually kill a dropped form arm. The host-absolute
/// spelling (POSIX `/…` on unix, drive `C:\…` on Windows) covers the key's
/// absolute rule and resolve's form arm on each leg; the root-relative
/// (`\…`) spelling covers the Windows RootDir shape; the verbatim
/// (`\\?\…`) spelling covers the UNC shape. (A `\…` spelling is a NAME at
/// the key layer — its key pin lives in the drive row.)
fn assert_in_vault_absolute_rejected(root: &Path) {
    write(root, "a/b.md", "x");
    let abs = root.join("a").join("b.md");
    let abs_text = abs.to_string_lossy().into_owned();
    assert!(
        matches!(
            note_source_key(&abs_text),
            Err(momotaro_contracts::SourceKeyError::Absolute(_))
        ),
        "key layer must refuse the host-absolute spelling of an in-vault file: {abs_text:?}"
    );
    assert!(
        matches!(
            resolve_local_path(root, &abs_text),
            Err(IngestError::OutsideWorkspace(_))
        ),
        "resolve must refuse it too, containment notwithstanding: {abs_text:?}"
    );
    let verbatim_abs = format!(r"\\?\{abs_text}");
    assert!(
        matches!(
            note_source_key(&verbatim_abs),
            Err(momotaro_contracts::SourceKeyError::Absolute(_))
        ),
        "key layer must refuse the verbatim spelling of an in-vault file: {verbatim_abs:?}"
    );
    #[cfg(windows)]
    {
        assert!(
            matches!(
                resolve_local_path(root, &verbatim_abs),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "resolve must refuse the verbatim spelling too: {verbatim_abs:?}"
        );
        let after_drive = abs_text.find(':').map(|i| i + 1).expect("drive path");
        let root_relative = &abs_text[after_drive..];
        assert!(
            matches!(
                resolve_local_path(root, root_relative),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "resolve must refuse the root-relative spelling too: {root_relative:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Path injection: junction, `..`, and the three absolute forms.
// (Card rows 1–5; vault ⊆ workspace contract, not in the 11.)
// ---------------------------------------------------------------------------

/// A link is a recorded entry, never an escape and never a second copy of the
/// content behind it.
#[test]
fn junction_is_recorded_and_never_an_escape() {
    let root = vault();
    let outside = vault();
    fs::write(outside.path().join("secret.md"), "s").expect("write outside");
    make_link(root.path(), "link", outside.path());

    // Discovery: the link is classified by its own type and not followed.
    let entries = walk_entries(root.path()).expect("walk");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].kind, momotaro_ingest::VaultFileKind::Symlink);
    // …and it keeps an identity as a path.
    assert_eq!(
        source_key_for(root.path(), &entries[0].path).expect("key"),
        "note:link"
    );

    // Resolution: neither reader nor writer may follow it out.
    assert!(
        matches!(
            resolve_local_path(root.path(), "link/secret.md"),
            Err(IngestError::OutsideWorkspace(_))
        ),
        "a link must not walk a stored path out of the workspace"
    );
    assert!(
        matches!(
            local_path_in(
                &canonical(root.path()),
                &root.path().join("link").join("secret.md")
            ),
            Err(IngestError::OutsideWorkspace(_))
        ),
        "and the writer must not produce such a value either"
    );
    // Containment through the link is false here — this is the automatic
    // witness that `is_within` compares canonicalized paths, not prefixes
    // (a lexical implementation would answer true on this fixture).
    assert!(
        !is_within(root.path(), &root.path().join("link").join("secret.md")).expect("containment"),
        "the link escapes; lexically it would look inside"
    );
}

/// A `..` component is rejected, never stripped and re-validated.
#[test]
fn parent_dir_injection_is_rejected_everywhere() {
    let root = vault();
    write(root.path(), "a/b.md", "x");

    for tail in ["..", "../x.md", "a/../x.md", "a/../../x.md", "./../x.md"] {
        assert!(
            matches!(
                note_source_key(tail),
                Err(momotaro_contracts::SourceKeyError::ParentDir)
            ),
            "key layer must reject {tail:?}"
        );
        assert!(
            matches!(
                resolve_local_path(root.path(), tail),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "resolve must reject {tail:?}"
        );
    }

    // The walker's own component pass rejects it too, before any key is built.
    let escaped = root.path().join("..").join("x.md");
    assert!(
        matches!(
            source_key_for(root.path(), &escaped),
            Err(IngestError::OutsideVault(_))
        ),
        "a path with a parent component is not a vault-relative name"
    );
}

/// POSIX-absolute tails never get a key and never resolve (card: 绝对注入 POSIX).
#[test]
fn posix_absolute_forms_are_rejected() {
    let root = vault();
    for tail in ["/etc/passwd", "/", "/x.md"] {
        assert!(
            matches!(
                note_source_key(tail),
                Err(momotaro_contracts::SourceKeyError::Absolute(_))
            ),
            "key layer must reject {tail:?}"
        );
        assert!(
            matches!(
                resolve_local_path(root.path(), tail),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "resolve must reject {tail:?}"
        );
    }

    assert_in_vault_absolute_rejected(root.path());
}

/// Drive-absolute tails (`C:\…`, `C:/…`) never get a key; the resolve layer
/// rejects them per OS semantics (card: 绝对注入 盘符).
///
/// The two layers answer different questions and the split is deliberate: the
/// key rule is platform-independent strings (a POSIX file may be *named*
/// `C:\x` and is still refused a key — conservative by contract), while
/// `resolve_local_path` speaks the host's path grammar. The unix arm pins that
/// divergence instead of pretending it is not there.
#[test]
fn drive_absolute_forms_are_rejected() {
    let root = vault();
    for tail in [r"C:\ws\a.md", "C:/ws/a.md", r"c:\a.md", "z:/a.md"] {
        assert!(
            matches!(
                note_source_key(tail),
                Err(momotaro_contracts::SourceKeyError::Absolute(_))
            ),
            "key layer must reject {tail:?}"
        );
    }
    // A drive-*relative* tail is not drive-absolute: it is kept verbatim as a
    // name at the key layer (guessing would be a rename). A lone leading
    // backslash is a name byte too — both pins run on every platform, the key
    // rules being platform-independent strings.
    assert_eq!(note_source_key("C:a.md").expect("name"), "note:C:a.md");
    assert_eq!(note_source_key(r"\a.md").expect("name"), r"note:\a.md");

    assert_in_vault_absolute_rejected(root.path());

    #[cfg(windows)]
    for tail in [r"C:\ws\a.md", "C:/ws/a.md", "C:a.md", r"\a.md"] {
        assert!(
            matches!(
                resolve_local_path(root.path(), tail),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "resolve must reject {tail:?} on Windows (prefix or root component)"
        );
    }
    #[cfg(unix)]
    {
        // POSIX has no drive letters: `C:\ws\a.md` is a legal file *name*
        // here, and the key layer still refuses to key it (see above).
        assert!(
            resolve_local_path(root.path(), r"C:\ws\a.md").is_ok(),
            "on POSIX a drive-shaped spelling is a name, not a path"
        );
    }
}

/// UNC tails (`\\server\share\…`, `\\?\…`) never get a key and never resolve
/// (card: 绝对注入 UNC).
#[test]
fn unc_absolute_forms_are_rejected() {
    let root = vault();
    for tail in [r"\\srv\share\a.md", r"\\?\C:\a.md", r"\\"] {
        assert!(
            matches!(
                note_source_key(tail),
                Err(momotaro_contracts::SourceKeyError::Absolute(_))
            ),
            "key layer must reject {tail:?}"
        );
    }

    assert_in_vault_absolute_rejected(root.path());

    #[cfg(windows)]
    for tail in [r"\\srv\share\a.md", r"\\?\C:\a.md"] {
        assert!(
            matches!(
                resolve_local_path(root.path(), tail),
                Err(IngestError::OutsideWorkspace(_))
            ),
            "resolve must reject {tail:?} on Windows (UNC prefix)"
        );
    }
    #[cfg(unix)]
    {
        // On POSIX, `\\srv\share\a.md` is one name whose bytes include both
        // backslashes — and the key layer still refuses it (UNC form).
        assert!(
            resolve_local_path(root.path(), r"\\srv\share\a.md").is_ok(),
            "on POSIX a UNC-shaped spelling is a name, not a path"
        );
    }
}

// ---------------------------------------------------------------------------
// Key layer: case, normalization, length, folding.
// (Card rows 6–10, 13–14 = items 1–6, 9–10.)
// ---------------------------------------------------------------------------

/// Case is preserved verbatim at the key layer: on a case-sensitive volume the
/// twins are two files and two identities (Linux 大小写双写合法并存,
/// 折叠 = 数据丢失级事故).
#[test]
fn case_double_write_stays_two_identities() {
    // The key half runs everywhere: no casefold, ever.
    assert_eq!(note_source_key("A.md").expect("key"), "note:A.md");
    assert_eq!(note_source_key("a.md").expect("key"), "note:a.md");
    assert_ne!(
        note_source_key("A.md").expect("key"),
        note_source_key("a.md").expect("key"),
        "a casefold in the key layer would merge two Linux files"
    );

    // The fixture half is what the platform can hold — not where the logic
    // runs. A case-sensitive volume holds both twins; NTFS looks up names
    // case-insensitively, so the second write lands on the first entry and the
    // walk sees one file whose spelling the key keeps.
    #[cfg(unix)]
    {
        let root = vault();
        write(root.path(), "A.md", "upper");
        write(root.path(), "a.md", "lower");
        assert_eq!(keys_of(root.path()), vec!["note:A.md", "note:a.md"]);
    }
    #[cfg(windows)]
    {
        let root = vault();
        write(root.path(), "A.md", "upper");
        write(root.path(), "a.md", "lower");
        let entries = walk_entries(root.path()).expect("walk");
        assert_eq!(entries.len(), 1, "one NTFS entry: {entries:?}");
        assert_eq!(
            source_key_for(root.path(), &entries[0].path).expect("key"),
            "note:A.md",
            "the surviving entry keeps the spelling it was created with"
        );
    }
}

/// A rename is a new identity — including a case-only rename.
#[test]
fn case_only_rename_is_a_new_identity() {
    let root = vault();
    write(root.path(), "A.md", "x");

    let before = walk_entries(root.path()).expect("walk");
    assert_eq!(before.len(), 1, "{before:?}");
    let key_before = source_key_for(root.path(), &before[0].path).expect("key");
    // The raw spelling comes from the product's `raw_name_for`, not from the
    // test's own reading of the path — a case-losing implementation must go
    // red here.
    let raw_before = raw_name_for(root.path(), &before[0].path).expect("raw name");
    assert_eq!(key_before, "note:A.md");
    assert_eq!(raw_before, "A.md");

    fs::rename(root.path().join("A.md"), root.path().join("a.md")).expect("rename");

    let after = walk_entries(root.path()).expect("walk");
    assert_eq!(after.len(), 1, "{after:?}");
    let key_after = source_key_for(root.path(), &after[0].path).expect("key");
    let raw_after = raw_name_for(root.path(), &after[0].path).expect("raw name");
    assert_eq!(key_after, "note:a.md");
    assert_eq!(raw_after, "a.md");

    assert_ne!(
        key_before, key_after,
        "path is identity: a case-only rename is a new source, not a refresh"
    );
}

/// An NFD name keys as its NFC form, and an NFC/NFD double write is a key
/// collision — two files, one key. The store-side treatment (两条目都
/// 保留 + 消歧后缀 + 事件) is the contract's destination and 未实现: today's
/// indexer refuses the second claimant with `reason_code = key_collision`, and
/// the suffix policy is 未实现. This row pins the
/// collision, not its resolution.
///
/// Fixture tier: NTFS stores UTF-16 without normalizing, so both legs can
/// create the twins (macOS normalizes and would collapse them — not a leg
/// here).
#[test]
fn nfd_names_key_as_nfc_and_twins_collide() {
    let root = vault();
    write(root.path(), "caf\u{e9}.md", "nfc");
    write(root.path(), "cafe\u{301}.md", "nfd");

    let entries = walk_entries(root.path()).expect("walk");
    assert_eq!(entries.len(), 2, "{entries:?}");
    let raws: Vec<String> = entries
        .iter()
        .map(|entry| raw_name_for(root.path(), &entry.path).expect("raw name"))
        .collect();
    // Sorted by path: the NFD spelling sorts first (plain `e` < `é`).
    assert_eq!(
        raws,
        vec!["cafe\u{301}.md", "caf\u{e9}.md"],
        "raw spellings stay distinct"
    );
    assert_eq!(
        keys_of(root.path()),
        vec!["note:caf\u{e9}.md", "note:caf\u{e9}.md"],
        "both spellings key as the NFC form — the collision the store must not merge"
    );

    // The single-file half: an NFD name alone still keys as NFC.
    assert_eq!(
        note_source_key("cafe\u{301}.md").expect("key"),
        "note:caf\u{e9}.md"
    );
}

/// A vault whose absolute path crosses 260 characters still keys exactly.
///
/// Windows: creation and the walk go through the `\\?\` verbatim prefix, the
/// form that works whether or not the volume has LongPathsEnabled — so the
/// fixture exists on any machine, not just ones with the registry key set.
#[test]
fn paths_past_260_key_exactly() {
    let base = tempfile::tempdir().expect("temp base");
    // Two long components plus a file: per-component under the 255 bound,
    // total well past 260.
    let long1 = "n".repeat(120);
    let long2 = "m".repeat(120);
    let rel = format!("{long1}/{long2}/deep-note.md");

    let root = verbatim(base.path());
    write(&root, &rel, "deep");

    let spelled_out = root.join(&long1).join(&long2).join("deep-note.md");
    let rendered = spelled_out.to_string_lossy().into_owned();
    assert!(
        rendered.len() > 260,
        "fixture must cross 260 chars to mean anything: {}",
        rendered.len()
    );

    let entries = walk_entries(&root).expect("walk");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(
        source_key_for(&root, &entries[0].path).expect("key"),
        format!("note:{rel}"),
        "the key is the exact relative spelling — no truncation, no folding"
    );
}

/// Reserved names and trailing dots/spaces are warned about (the predicate is
/// platform-independent — the rules Explorer and sync engines enforce), and
/// the key keeps such names byte-for-byte: the row indexes, it warns, it never
/// rewrites.
#[test]
fn reserved_names_warn_and_keep_their_bytes() {
    // The warning predicate: reserved device names (first segment before `.`,
    // ASCII-insensitive, superscript digits included) and trailing `.` / ` `.
    for name in [
        "con.md",
        "CON.MD",
        "nul.txt",
        "com1.a.b",
        "lpt9",
        "com\u{b9}.md",
    ] {
        assert_eq!(
            windows_incompatible_components(name),
            vec![name.to_string()],
            "{name:?} is reserved"
        );
        assert_eq!(
            note_source_key(name).expect("key"),
            format!("note:{name}"),
            "and still keyed byte-exact"
        );
    }
    for name in [
        "constitution.md",
        "com10.md",
        "aux2.md",
        "trail .md",
        "sp ace.md",
    ] {
        assert!(
            windows_incompatible_components(name).is_empty(),
            "{name:?} is legal — prefix matching would be wrong"
        );
    }
    for name in ["foo.", "foo "] {
        assert_eq!(
            windows_incompatible_components(name),
            vec![name.to_string()],
            "{name:?} ends in a trailing dot or space"
        );
        assert_eq!(note_source_key(name).expect("key"), format!("note:{name}"));
    }

    // Fixture half: real hostile names as the platform can create them.
    // POSIX creates all of them natively; Windows creates the reserved names
    // (measured: std does on Windows 11) and the trailing-dot name through the
    // verbatim prefix. The reserved-character names (`a|b.md` …) are POSIX
    // fixtures — Win32 refuses them at the API surface the walk uses.
    #[cfg(unix)]
    {
        let root = vault();
        let expected = [
            ("con.md", "note:con.md"),
            ("foo.", "note:foo."),
            ("a|b.md", "note:a|b.md"),
            ("a:b.md", "note:a:b.md"),
        ];
        for (name, _) in expected {
            write(root.path(), name, "x");
        }
        let entries = walk_entries(root.path()).expect("walk");
        assert_eq!(entries.len(), expected.len(), "{entries:?}");
        for (name, key) in expected {
            let path = root.path().join(name);
            assert_eq!(
                raw_name_for(root.path(), &path).expect("raw name"),
                name,
                "{name:?} must round-trip with its literal spelling"
            );
            assert_eq!(
                source_key_for(root.path(), &path).expect("key"),
                key,
                "{name:?} must key as its literal bytes"
            );
            assert!(
                !windows_incompatible_components(name).is_empty(),
                "{name:?} must be warned about"
            );
        }
    }
    #[cfg(windows)]
    {
        let base = tempfile::tempdir().expect("temp base");
        let root = verbatim(base.path());
        let expected = [("con.md", "note:con.md"), ("foo.", "note:foo.")];
        for (name, _) in expected {
            write(&root, name, "x");
        }
        let entries = walk_entries(&root).expect("walk");
        assert_eq!(entries.len(), expected.len(), "{entries:?}");
        for (name, key) in expected {
            let path = root.join(name);
            assert_eq!(
                raw_name_for(&root, &path).expect("raw name"),
                name,
                "{name:?} must round-trip with its literal spelling"
            );
            assert_eq!(
                source_key_for(&root, &path).expect("key"),
                key,
                "{name:?} must key as its literal bytes"
            );
            assert!(!windows_incompatible_components(name).is_empty());
        }
    }
}

/// Three spellings of one path are one key: the equivalence class is NFC,
/// `.`/empty components and a trailing separator — and nothing else.
#[test]
fn three_spellings_of_one_path_are_one_key() {
    let canonical_spelling = "a/b.md";
    let trio = [canonical_spelling, "./a//b.md", "a/b.md/"];
    let keys: Vec<String> = trio
        .iter()
        .map(|tail| note_source_key(tail).expect("key"))
        .collect();
    assert_eq!(
        keys,
        vec!["note:a/b.md"; 3],
        "the `.`/empty/trailing-separator spellings collapse; nothing else does"
    );

    // Filesystem half: one real file, and every spelling this platform's path
    // grammar can actually name it with resolves to it — equal paths, same
    // bytes, one `local_path`.
    //
    // The trailing-slash spelling is a *key* equivalence (asserted above), not
    // an OS path fact: POSIX answers `b.md/` with ENOTDIR because a trailing
    // slash demands a directory, while on Windows the resolver's join
    // normalizes the spelling before anything opens the file (raw Win32 keeps
    // the slash — measured). Neither is a gap — the equivalence class governs key
    // construction, and `resolve_local_path` resolves stored `local_path`
    // values, which never carry a trailing slash. Both platform answers are
    // pinned below so the divergence is a recorded fact, not a surprise.
    let root = vault();
    write(root.path(), canonical_spelling, "x");
    let canonical_root = canonical(root.path());
    let mut resolved_paths = Vec::new();
    for spelling in [canonical_spelling, "./a//b.md"] {
        let resolved = resolve_local_path(root.path(), spelling).expect(spelling);
        assert_eq!(
            fs::read(&resolved).expect("the resolved path must open"),
            b"x",
            "{spelling:?} must resolve to the same file"
        );
        assert_eq!(
            local_path_in(&canonical_root, &resolved).expect("local_path"),
            canonical_spelling,
            "{spelling:?} must render as the canonical spelling"
        );
        resolved_paths.push(resolved);
    }
    assert_eq!(
        resolved_paths[0], resolved_paths[1],
        "the spellings resolve to one path: {resolved_paths:?}"
    );

    #[cfg(windows)]
    {
        let resolved = resolve_local_path(root.path(), "a/b.md/").expect("trailing slash");
        assert_eq!(fs::read(&resolved).expect("open"), b"x");
        assert_eq!(
            resolved, resolved_paths[0],
            "the join normalizes the spelling to the canonical path (raw Win32 keeps the slash)"
        );
    }
    #[cfg(unix)]
    {
        assert!(
            matches!(
                resolve_local_path(root.path(), "a/b.md/"),
                Err(IngestError::Io(_))
            ),
            "POSIX gives a trailing-slash spelling of a regular file exactly one \
             honest answer — a resolver that accepted it would be rewriting the \
             platform's grammar"
        );
    }

    // The boundary the contract draws: a backslash inside a NAME is a name
    // byte, so these are two identities — folding it to `/` would merge them.
    assert_ne!(
        note_source_key(r"docs\ml\a.md").expect("name"),
        note_source_key("docs/ml/a.md").expect("name")
    );
}

/// The folding-trap characters pin both edges of the NFC key rule (
/// 禁用 NFKC 与 casefold; key = NFC identity).
///
/// Two behaviors, deliberately different:
///
/// - `İ`/`I`/`i`, `Σ`/`σ`/`ς`, `Maße`/`Masse` are **casefold traps** — a
///   forbidden casefold merges each pair, so their keys must stay distinct.
///   (NFKC agrees with NFC on every pin in this test; the NFKC fold the key
///   must not do is what the `com¹.md` byte pin in the reserved-names row
///   catches.)
/// - `者` (U+8005) / `者` (U+FA5B) is the folding pair; U+FA5B is a
///   canonical **singleton decomposition** of U+8005 (UnicodeData: `FA5B …
///   8005`), so plain NFC — the key rule itself — already maps both spellings
///   onto one key. It is the same class as the café NFC/NFD twins: an NFC
///   collision, not a distinct-key pair. The store-side treatment (两条目都
///   保留 + 消歧后缀 + 事件) is the contract's destination and 未实现
///   — today's indexer refuses the second claimant with `key_collision`.
///   Dropping NFC for byte-identity keys would *split* this pair — the
///   mutation these assertions kill.
#[test]
fn fold_traps_stay_distinct_and_the_nfc_pair_collides() {
    let pinned_distinct_pairs = [
        ("\u{130}.md", "I.md"),       // İ vs I — the Turkish dotted-I trap
        ("\u{130}.md", "i.md"),       // İ vs i
        ("\u{3a3}.md", "\u{3c3}.md"), // Σ vs σ — casefold trap
        ("\u{3a3}.md", "\u{3c2}.md"), // Σ vs ς — final sigma
        ("\u{3c3}.md", "\u{3c2}.md"), // σ vs ς
        ("Ma\u{df}e.md", "Masse.md"), // Maße vs Masse — casefold ß→ss
    ];
    for (left, right) in pinned_distinct_pairs {
        assert_ne!(
            note_source_key(left).expect("key"),
            note_source_key(right).expect("key"),
            "{left:?} and {right:?} must never share a key"
        );
    }
    // NFC, and only NFC: `İ` decomposes canonically to `I` + combining dot,
    // so that spelling must recompose to `İ` — while staying distinct from `I`.
    assert_eq!(
        note_source_key("I\u{307}.md").expect("key"),
        "note:\u{130}.md"
    );

    // The pair NFC itself merges: one key for both spellings, and the key is
    // the U+8005 form (labels in the messages because both render as 者).
    assert_eq!(
        note_source_key("\u{fa5b}.md").expect("key"),
        note_source_key("\u{8005}.md").expect("key"),
        "U+FA5B is a canonical singleton decomposition of U+8005: NFC maps both to one key"
    );
    assert_eq!(
        note_source_key("\u{fa5b}.md").expect("key"),
        "note:\u{8005}.md",
        "the key is the composed U+8005 form"
    );

    // Filesystem half: every name the platform can hold maps to its expected
    // key. A volume's own lookup table decides which *twins* can share it —
    // NTFS folds the sigma forms and `i`/`I` onto one entry each (measured by
    // this fixture on Windows 11) — so those twins are POSIX-only fixtures.
    // That is fixture semantics, not key semantics: the pins above run
    // everywhere. The U+8005/U+FA5B files coexist on every platform (no case
    // relation) and land on one key — walked raw-name-by-raw-name below.
    fn assert_keyed_names(root: &Path, expected: &[(&str, &str)]) {
        let walked: BTreeMap<String, String> = walk_entries(root)
            .expect("walk")
            .into_iter()
            .map(|entry| {
                let raw = raw_name_for(root, &entry.path).expect("raw name");
                let key = source_key_for(root, &entry.path).expect("key");
                (raw, key)
            })
            .collect();
        let expected: BTreeMap<String, String> = expected
            .iter()
            .map(|(raw, key)| ((*raw).to_string(), (*key).to_string()))
            .collect();
        assert_eq!(walked, expected);
    }

    #[cfg(unix)]
    {
        let root = vault();
        let expected = [
            ("\u{130}.md", "note:\u{130}.md"),
            ("I.md", "note:I.md"),
            ("i.md", "note:i.md"),
            ("\u{3a3}.md", "note:\u{3a3}.md"),
            ("\u{3c3}.md", "note:\u{3c3}.md"),
            ("\u{3c2}.md", "note:\u{3c2}.md"),
            ("Ma\u{df}e.md", "note:Ma\u{df}e.md"),
            ("Masse.md", "note:Masse.md"),
            ("\u{8005}.md", "note:\u{8005}.md"),
            // Both raw spellings, one key: the NFC collision, witnessed on disk.
            ("\u{fa5b}.md", "note:\u{8005}.md"),
        ];
        for (name, _) in expected {
            write(root.path(), name, "x");
        }
        assert_keyed_names(root.path(), &expected);
    }
    #[cfg(windows)]
    {
        let base = tempfile::tempdir().expect("temp base");
        let root = verbatim(base.path());
        // Without `i.md`, `σ.md`, `ς.md`: NTFS shares one entry for `σ`/`ς`/`Σ`
        // and one for `i`/`I`.
        let expected = [
            ("\u{130}.md", "note:\u{130}.md"),
            ("I.md", "note:I.md"),
            ("\u{3a3}.md", "note:\u{3a3}.md"),
            ("Ma\u{df}e.md", "note:Ma\u{df}e.md"),
            ("Masse.md", "note:Masse.md"),
            ("\u{8005}.md", "note:\u{8005}.md"),
            ("\u{fa5b}.md", "note:\u{8005}.md"),
        ];
        for (name, _) in expected {
            write(&root, name, "x");
        }
        assert_keyed_names(&root, &expected);
    }
}

// ---------------------------------------------------------------------------
// Sync-disk boundaries (OneDrive + iCloud — the third of the three,
// the Windows-illegal-name warning, is the reserved-names row above).
// (Card rows 11–12 = items 7–8.)
// ---------------------------------------------------------------------------

/// The criterion is exactly one bit, and an ordinary file lacks it: the
/// read-time gate must not trip on plain files (判据只认
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`).
///
/// The bit-level pins run everywhere (`is_cloud_placeholder` is a pure
/// function on the attribute word); the second half drives the real attribute
/// read on whatever this platform has.
#[test]
fn placeholder_criterion_is_one_bit_and_ordinary_files_lack_it() {
    // WinNT.h values, fed as raw words: only RECALL_ON_DATA_ACCESS counts.
    assert!(
        is_cloud_placeholder(0x0040_0000),
        "RECALL_ON_DATA_ACCESS is the criterion"
    );
    assert!(
        !is_cloud_placeholder(0x0000_1000),
        "OFFLINE is settable on an ordinary file (attrib +o, measured)"
    );
    assert!(
        !is_cloud_placeholder(0x0004_0000),
        "RECALL_ON_OPEN is not observable through a handle read"
    );

    let root = vault();
    write(root.path(), "note.md", "x");
    let attrs = attributes_of(&root.path().join("note.md")).expect("attributes");
    assert!(
        !is_cloud_placeholder(attrs),
        "an ordinary file is not a placeholder: attrs={attrs:#x}"
    );
}

/// The one thing no fixture can build: a real cloud placeholder (the bit is
/// kernel-settable only). Run it by hand once — sign in to OneDrive, turn on
/// "Files On-Demand", right-click a file → "Free up space", and point
/// `MOMOTARO_ONEDRIVE_SAMPLE` at it. Run it per the file-header recipe with
/// `--ignored a_real_onedrive_placeholder_is_recognized_and_plain_to_the_walk`.
///
/// It stops with these steps when the variable is unset, so "did not run"
/// cannot read as "passed". What this proves beyond the injected-attribute
/// index tests: the reader sees the bit through `fs::metadata` on a real
/// sample, and the walk lists such a file as an ordinary file — placeholder
/// -ness is a read-time fact, not a walk kind (the check sits after
/// `fs::metadata`, before any read).
#[cfg(windows)]
#[test]
#[ignore = "needs a real OneDrive placeholder (MOMOTARO_ONEDRIVE_SAMPLE)"]
fn a_real_onedrive_placeholder_is_recognized_and_plain_to_the_walk() {
    let Some(sample) = std::env::var_os("MOMOTARO_ONEDRIVE_SAMPLE") else {
        panic!(
            "set MOMOTARO_ONEDRIVE_SAMPLE to a real cloud placeholder: sign in to \
             OneDrive, turn on \"Files On-Demand\", right-click a file → \"Free up \
             space\", and point this at it — then run this test again"
        );
    };
    let path = PathBuf::from(sample);
    let attrs = attributes_of(&path).expect("read the sample's attributes (handle only)");
    assert!(
        is_cloud_placeholder(attrs),
        "the sample must carry FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: attrs={attrs:#x}"
    );

    // The walk sees an ordinary file: the gate lives at read time, not in
    // discovery.
    let parent = path.parent().expect("the sample has a parent directory");
    let entries = walk_entries(parent).expect("walk the sample's folder");
    let found = entries
        .iter()
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("the sample must appear in the walk of its folder: {path:?}"));
    assert_ne!(
        found.kind,
        momotaro_ingest::VaultFileKind::Symlink,
        "a placeholder is a regular file to discovery"
    );
}

/// An iCloud eviction stub is classified so a row can say what happened to the
/// note it stands for — and its content never enters the index.
#[test]
fn icloud_stubs_are_classified_and_never_indexed() {
    let root = vault();
    write(root.path(), ".note.md.icloud", "stub");
    write(root.path(), "note.md.icloud", "suffix-only control");
    write(root.path(), "x.icloud.md", "real note");
    write(root.path(), ".x.icloud.md", "hidden real note");

    let entries = walk_entries(root.path()).expect("walk");
    let kinds: Vec<(String, momotaro_ingest::VaultFileKind)> = entries
        .iter()
        .map(|entry| {
            (
                entry
                    .path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
                entry.kind,
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            (
                ".note.md.icloud".to_string(),
                momotaro_ingest::VaultFileKind::IcloudPlaceholder
            ),
            // No leading dot: the suffix alone does not make a stub — a
            // suffix-only implementation would classify this one as a stub.
            (
                "note.md.icloud".to_string(),
                momotaro_ingest::VaultFileKind::Attachment
            ),
            (
                "x.icloud.md".to_string(),
                momotaro_ingest::VaultFileKind::Note
            ),
        ],
        "the stub is kept as a fact; the hidden real note stays hidden"
    );

    // The stub keeps an identity for its row…
    assert_eq!(
        source_key_for(root.path(), &root.path().join(".note.md.icloud")).expect("key"),
        "note:.note.md.icloud"
    );
    // …and is not indexable content.
    let indexable: Vec<String> = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                momotaro_ingest::VaultFileKind::Note | momotaro_ingest::VaultFileKind::Canvas
            )
        })
        .map(|entry| {
            entry
                .path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(indexable, vec!["x.icloud.md".to_string()]);
}

// ---------------------------------------------------------------------------
// Manual tier (card rows 15–17; row 11's manual half is up in the
// sync-disk section): the states no fixture can fabricate.
// Each panics with setup steps when its env var is unset. All Windows-only;
// run one at a time by name (see the file header).
// ---------------------------------------------------------------------------

/// A vault on a real exFAT volume (a USB stick will do): exFAT folds case
/// through its upcase table but does not normalize Unicode, so the NFC/NFD
/// twins must coexist with one key, and the case twins must share one entry
/// whose spelling the key keeps. Fixture: an **empty** folder on the exFAT
/// volume; point `MOMOTARO_EXFAT_ROOT` at it. Run it per the file-header
/// recipe with `--ignored exfat_vault_keys_stay_byte_exact`.
#[cfg(windows)]
#[test]
#[ignore = "needs a real exFAT volume (MOMOTARO_EXFAT_ROOT)"]
fn exfat_vault_keys_stay_byte_exact() {
    let Some(root) = std::env::var_os("MOMOTARO_EXFAT_ROOT") else {
        panic!(
            "set MOMOTARO_EXFAT_ROOT to an empty folder on a real exFAT volume \
             (a USB stick formatted exFAT will do) — this test creates and \
             removes its fixtures there"
        );
    };
    let root = PathBuf::from(root);
    assert!(
        root.is_dir(),
        "MOMOTARO_EXFAT_ROOT must be an existing directory: {root:?}"
    );
    assert!(
        fs::read_dir(&root)
            .expect("read the folder")
            .next()
            .is_none(),
        "MOMOTARO_EXFAT_ROOT must be an EMPTY folder — this test writes and then          deletes fixed names there and will not touch anything it did not          create: {root:?}"
    );
    // Self-verify the premise: every assertion below would also hold on NTFS,
    // and a green run that did not touch exFAT is exactly the false green this
    // file forbids. Filesystem names are not localized (the labels are).
    let drive_root = format!("{}\\", &root.to_string_lossy()[..2]);
    let fsinfo = std::process::Command::new("cmd")
        .arg("/C")
        .arg(format!("fsutil fsinfo volumeinfo {drive_root}"))
        .output()
        .expect("run fsutil");
    let info = format!(
        "{}{}",
        String::from_utf8_lossy(&fsinfo.stdout),
        String::from_utf8_lossy(&fsinfo.stderr)
    );
    assert!(
        info.to_uppercase().contains("EXFAT"),
        "MOMOTARO_EXFAT_ROOT must sit on an exFAT volume — fsutil said: {info:?}"
    );

    // NFC/NFD twins: distinct directory entries on exFAT (no normalization).
    write(&root, "caf\u{e9}.md", "nfc");
    write(&root, "cafe\u{301}.md", "nfd");
    // Case twins: ONE entry — the second write lands on the first.
    write(&root, "Twin.md", "upper");
    write(&root, "twin.md", "lower");

    let entries = walk_entries(&root).expect("walk the exFAT folder");
    let ours: Vec<(String, String)> = entries
        .iter()
        .filter(|entry| {
            let name = entry
                .path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            name.ends_with(".md")
        })
        .map(|entry| {
            (
                entry
                    .path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned(),
                source_key_for(&root, &entry.path).expect("key"),
            )
        })
        .collect();

    let twins: Vec<&(String, String)> = ours
        .iter()
        .filter(|(raw, _)| raw.eq_ignore_ascii_case("twin.md"))
        .collect();
    assert_eq!(twins.len(), 1, "case twins share one exFAT entry: {ours:?}");
    assert_eq!(
        twins[0].0, "Twin.md",
        "the surviving entry keeps the first spelling: {ours:?}"
    );
    assert_eq!(twins[0].1, "note:Twin.md");
    assert_eq!(
        fs::read_to_string(root.join("Twin.md")).expect("read"),
        "lower",
        "the second write landed on the same entry — that is the case-insensitivity"
    );

    let nfc_twins: Vec<&(String, String)> = ours
        .iter()
        .filter(|(_, key)| key == "note:caf\u{e9}.md")
        .collect();
    assert_eq!(
        nfc_twins.len(),
        2,
        "NFC and NFD spellings coexist on exFAT and collide in the key: {ours:?}"
    );

    // Leave the folder as found. Cleanup is tail-only: on a failed assertion
    // the three fixtures stay behind — the folder was empty, so nothing of the
    // user's can be lost with them.
    for name in ["caf\u{e9}.md", "cafe\u{301}.md", "Twin.md"] {
        fs::remove_file(root.join(name)).expect("remove the fixture this test created");
    }
}

/// A junction to another volume is not an escape either — containment is
/// decided between canonicalized paths, and a different volume never starts
/// with the workspace root. Fixture: a second **writable** volume (e.g. a USB
/// stick); point `MOMOTARO_SECOND_VOLUME` at an empty folder on it. Run it
/// per the file-header recipe with `--ignored cross_volume_link_is_not_an_escape`.
#[cfg(windows)]
#[test]
#[ignore = "needs a second volume (MOMOTARO_SECOND_VOLUME)"]
fn cross_volume_link_is_not_an_escape() {
    let Some(other) = std::env::var_os("MOMOTARO_SECOND_VOLUME") else {
        panic!(
            "set MOMOTARO_SECOND_VOLUME to an empty folder on a second volume \
             (a USB stick will do); this test writes one file there and links \
             it into a temp vault"
        );
    };
    let other = PathBuf::from(other);
    assert!(
        other.is_dir(),
        "MOMOTARO_SECOND_VOLUME must be an existing directory: {other:?}"
    );
    assert!(
        fs::read_dir(&other)
            .expect("read the folder")
            .next()
            .is_none(),
        "MOMOTARO_SECOND_VOLUME must be an EMPTY folder — this test writes and          then deletes `secret.md` there and will not touch anything it did not          create: {other:?}"
    );

    let root = vault();
    // Self-verify the premise: "cross-volume" means the two roots sit on
    // different disks, or this row would be testing the same-volume case.
    assert_ne!(
        drive_of(&canonical(root.path())),
        drive_of(&canonical(&other)),
        "MOMOTARO_SECOND_VOLUME must be on a different volume than TEMP"
    );
    fs::write(other.join("secret.md"), "s").expect("write on the second volume");
    make_link(root.path(), "link", &other);

    assert!(
        !is_within(root.path(), &root.path().join("link").join("secret.md")).expect("containment"),
        "a cross-volume link must not read as inside the workspace"
    );
    assert!(
        matches!(
            resolve_local_path(root.path(), "link/secret.md"),
            Err(IngestError::OutsideWorkspace(_))
        ),
        "and must not resolve a stored path out either"
    );

    // Tail-only cleanup; the folder was empty, so the file can only be ours.
    fs::remove_file(other.join("secret.md")).expect("remove the file this test created");
}

/// An 8.3 short spelling resolves to the long identity: `local_path` is
/// rendered from the canonicalized path, so the short form never leaks into a
/// stored value. Fixture: a volume that still generates 8.3 names
/// (`fsutil 8dot3name query <volume>`) — the test builds its own and stops
/// with these steps if the volume does not generate them. Run it per the
/// file-header recipe with `--ignored
/// eight_three_short_names_resolve_to_the_long_identity`.
#[cfg(windows)]
#[test]
#[ignore = "needs 8.3 short names enabled on the volume (fsutil 8dot3name query)"]
fn eight_three_short_names_resolve_to_the_long_identity() {
    let root = vault();
    write(root.path(), "a-rather-long-directory-name/notes.md", "x");
    let long_dir = root.path().join("a-rather-long-directory-name");

    let output = std::process::Command::new("cmd")
        .arg("/C")
        .arg(format!(
            "for %I in (\"{}\") do @echo %~sI",
            long_dir.display()
        ))
        .output()
        .expect("run the short-name probe");
    let short = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !short.is_empty() && output.status.success(),
        "the probe must return a path: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        PathBuf::from(&short),
        long_dir,
        "this volume does not generate 8.3 names — run on a volume that does \
         (check with `fsutil 8dot3name query <volume>`); enabling them is a \
         machine-wide setting and this test will not touch it"
    );

    let via_short = PathBuf::from(&short).join("notes.md");
    let canonical_root = canonical(root.path());
    assert_eq!(
        local_path_in(&canonical_root, &via_short).expect("local_path"),
        "a-rather-long-directory-name/notes.md",
        "the stored spelling is the long name — the short form is a way in, not an identity"
    );
    assert!(is_within(root.path(), &via_short).expect("containment"));
}

// ---------------------------------------------------------------------------
// Row 18: a path that does not exist yet (nearest existing ancestor).
// ---------------------------------------------------------------------------

/// A path that does not exist yet resolves through its nearest existing
/// ancestor: the missing tail is kept, not dropped (尚不存在的路径按
/// 「最近的已存在祖先」规范化后比较).
#[test]
fn not_yet_created_paths_resolve_through_the_nearest_existing_ancestor() {
    let root = vault();
    write(root.path(), "a/existing.md", "x");
    let canonical_root = canonical(root.path());

    // Two missing components below an existing directory.
    let missing = root.path().join("a").join("deep").join("future.md");
    assert_eq!(
        local_path_in(&canonical_root, &missing).expect("local_path"),
        "a/deep/future.md",
        "the missing tail is re-appended unchanged"
    );
    let resolved = resolve_local_path(root.path(), "a/deep/future.md").expect("resolve");
    assert!(
        resolved.ends_with(Path::new("a/deep/future.md")),
        "the resolved path keeps the whole tail: {}",
        resolved.display()
    );
    assert!(
        is_within(root.path(), &missing).expect("containment"),
        "a not-yet-created path inside the workspace is inside it"
    );

    // Outside stays outside even when nothing exists there either.
    let other = vault();
    let ghost = other.path().join("ghost.md");
    assert!(
        matches!(
            local_path_in(&canonical_root, &ghost),
            Err(IngestError::OutsideWorkspace(_))
        ),
        "a missing file outside the workspace is outside it"
    );
    assert!(
        !is_within(root.path(), &ghost).expect("containment"),
        "containment does not depend on existence"
    );
}
