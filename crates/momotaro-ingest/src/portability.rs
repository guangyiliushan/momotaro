//! What a vault file's name or attributes mean on another platform.
//!
//! One home for the three sync-disk boundaries: a Windows cloud placeholder
//! whose bytes are not on this device, an iCloud eviction stub standing in for
//! a note that is not here, and a name Windows' own naming rules reserve. The
//! name predicates are pure strings (both platforms compile and
//! test them); the attribute read is the one platform-specific piece, and it
//! comes as a pair of `cfg`-bound functions with one signature, so callers
//! never branch.

use std::path::Path;

/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` (WinNT.h): set on every cloud
/// placeholder — the IFS guidance requires it of all virtualization
/// implementations that use placeholders — and settable by kernel-mode callers
/// only, which is why no test on any platform can fabricate the state and the
/// reader below keeps it a plain value to test.
const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` (WinNT.h): the fully virtual neighbours of a
/// placeholder. It "only appears in directory enumeration classes", and the
/// handle-based metadata read this crate performs never sees it, so it is
/// documented as a neighbour rather than used as a criterion.
const RECALL_ON_OPEN: u32 = 0x0004_0000;

/// `FILE_ATTRIBUTE_OFFLINE`: Remote Storage / HSM, not cloud placeholders —
/// and settable on an ordinary file (`attrib +o`, measured), so using it as a
/// criterion would flag ordinary files.
const OFFLINE: u32 = 0x0000_1000;

/// `FILE_ATTRIBUTE_REPARSE_POINT`: links are classified by their own type in
/// the walk; this bit is never a placeholder criterion either.
const REPARSE_POINT: u32 = 0x0000_0400;

/// The bits a cloud placeholder is recognized by: exactly
/// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, and nothing else.
const PLACEHOLDER_BITS: u32 = RECALL_ON_DATA_ACCESS;

// Widening the criterion is a compile error rather than a review finding:
// including OFFLINE would flag ordinary files, REPARSE_POINT would flag links,
// and RECALL_ON_OPEN is not observable through the handle-based read, so
// keying on it would add a branch no platform can reach.
const _: () = assert!(PLACEHOLDER_BITS & (RECALL_ON_OPEN | OFFLINE | REPARSE_POINT) == 0);

/// Whether these Windows file attributes describe a cloud placeholder.
///
/// Only `RECALL_ON_DATA_ACCESS` is a criterion: the official guidance requires
/// every placeholder to carry it, while OFFLINE / PINNED / UNPINNED /
/// REPARSE_POINT are measured or documented to mean something else. Takes the
/// raw attribute value, so the verdict is a pure function both platforms can
/// test.
#[must_use]
pub fn is_cloud_placeholder(attrs: u32) -> bool {
    attrs & PLACEHOLDER_BITS != 0
}

/// The Windows file attributes of the file at `path`.
///
/// The read goes through `fs::metadata`, which opens with no access mode and
/// reads a handle's attributes rather than the file's bytes — nothing here
/// hydrates a placeholder, which is the whole point.
///
/// # Errors
///
/// - The `std::io::Error` from `fs::metadata` when the path cannot be read:
///   it does not exist, or the filesystem refuses access.
#[cfg(windows)]
pub fn attributes_of(path: &Path) -> std::io::Result<u32> {
    use std::os::windows::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.file_attributes())
}

/// The Windows file attributes of the file at `path`, or zero where the
/// platform has none.
///
/// There is no such attribute off Windows, and zero is the truthful answer —
/// which also costs no syscall, so callers can ask unconditionally.
///
/// # Errors
///
/// Infallible: this arm reads no filesystem state and always returns `Ok(0)`.
/// The `Result` is kept so callers see one signature on every platform.
#[cfg(not(windows))]
pub fn attributes_of(_path: &Path) -> std::io::Result<u32> {
    Ok(0)
}

/// Whether a file name is an iCloud eviction stub.
///
/// The criterion anchors both ends on purpose: `x.icloud.md` and
/// `.x.icloud.md` are ordinary notes whose names happen to contain `.icloud`,
/// and a substring or extension-only test would swallow them. The system
/// writes the suffix in lowercase, and the comparison is exact, so a case
/// variant Apple never produces stays the dot file it otherwise is.
#[must_use]
pub fn icloud_stub(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".icloud")
}

/// The 28 device names Win32 reserves, lowercase — `NUL.txt` is `NUL` with an
/// extension, and the note under the attribute table treats the superscript
/// digits ¹ ² ³ as digits of `COM#` / `LPT#`.
const RESERVED_NAMES: [&str; 28] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "com¹", "com²", "com³", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8",
    "lpt9", "lpt¹", "lpt²", "lpt³",
];

/// The nine characters Win32 reserves in a name: `<` `>` `:` `"` `/` `\` `|`
/// `?` `*`. A component can never contain `/` — it is the separator these
/// inputs are joined with — but the list is the rule as written.
const RESERVED_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The vault-relative components of `raw_name` that Windows' naming rules
/// reserve, in order; empty means the name is fine there.
///
/// Windows rules, applied per component and exactly ("Naming Files, Paths, and
/// Namespaces"): the segment before the first `.` equals (ASCII
/// case-insensitively, superscript digits included) one of the 28 reserved
/// device names; a reserved character or a NUL / 1–31 control character
/// appears; or the name ends in a dot or a space. Prefix matching would flag
/// `constitution.md` and comparing the whole name would miss `nul.txt` — both
/// measured on a real Windows machine while this was designed.
///
/// The verdict is platform-independent, and deliberately the conservative
/// reading of the documented rules: `\\?\`-style API access can create several
/// of these names even on Windows 11 (measured), but Explorer, shells and sync
/// engines enforce the rules — the name is what syncs, and the other end is
/// where it breaks.
#[must_use]
pub fn windows_incompatible_components(raw_name: &str) -> Vec<String> {
    raw_name
        .split('/')
        .filter(|component| windows_incompatible(component))
        .map(str::to_owned)
        .collect()
}

/// The rule for one component, `windows_incompatible_components`'s worker.
fn windows_incompatible(component: &str) -> bool {
    if component.is_empty() {
        return false;
    }
    // `NUL.txt` is `NUL` with an extension: only the first segment decides the
    // device-name rule.
    let first_segment = component
        .split_once('.')
        .map_or(component, |(head, _)| head);
    if RESERVED_NAMES.contains(&first_segment.to_ascii_lowercase().as_str()) {
        return true;
    }
    if component
        .chars()
        .any(|c| ('\0'..='\u{1f}').contains(&c) || RESERVED_CHARS.contains(&c))
    {
        return true;
    }
    // "Do not end a file or directory name with a space or a period."
    component.ends_with('.') || component.ends_with(' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn only_the_recall_on_data_access_bit_makes_a_placeholder() {
        // The criterion is the one bit the official guidance requires of every
        // placeholder; every neighbour bit that a measured or documented
        // lookalike state would set is asserted not to be part of it.
        assert!(!is_cloud_placeholder(0));
        assert!(!is_cloud_placeholder(0x0000_0020), "a new file (ARCHIVE)");
        assert!(is_cloud_placeholder(0x0040_0000));
        assert!(is_cloud_placeholder(0x0040_0020), "a placeholder on disk");
        assert!(
            is_cloud_placeholder(0x0044_0020),
            "with RECALL_ON_OPEN set as well"
        );
        assert!(
            !is_cloud_placeholder(0x0004_0020),
            "RECALL_ON_OPEN alone is not a criterion"
        );
        assert!(
            !is_cloud_placeholder(0x0000_1020),
            "OFFLINE is settable on an ordinary file (attrib +o, measured)"
        );
        assert!(!is_cloud_placeholder(0x0008_0020), "PINNED, user intent");
        assert!(!is_cloud_placeholder(0x0010_0020), "UNPINNED, user intent");
        assert!(!is_cloud_placeholder(0x0000_0420), "a reparse point");
    }

    #[test]
    fn an_ordinary_new_file_is_not_a_placeholder() {
        // The chain an index run takes — attributes_of then
        // is_cloud_placeholder — on a file this platform really has. On
        // Windows this reads the real attribute word; elsewhere there is none
        // and zero answers. A real placeholder cannot be fabricated anywhere
        // (the bit is kernel-settable only), so the positive verdicts live in
        // the pure test above and in the one ignored, manual test.
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("plain.md");
        fs::write(&file, "x").expect("write");
        let attrs = attributes_of(&file).expect("attributes");
        assert!(!is_cloud_placeholder(attrs), "attrs={attrs:#x}");
    }

    /// The production reader, not an injected one.
    ///
    /// Every other test here either hands `is_cloud_placeholder` a literal or
    /// injects `attributes_of`, so the real Windows arm can be replaced by
    /// either constant and stay green: `Ok(0)` and `Ok(1)` are both
    /// non-placeholders, and the only positive verdict is the `#[ignore]`d one.
    /// A file this machine just wrote carries `FILE_ATTRIBUTE_ARCHIVE`, which
    /// neither constant has — so this is the only assertion that can tell the
    /// reader reads.
    #[cfg(windows)]
    #[test]
    fn the_windows_reader_returns_the_files_own_attributes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("fresh.md");
        fs::write(&file, "x").expect("write");
        let attrs = attributes_of(&file).expect("attributes");
        assert_eq!(
            attrs & 0x0000_0020,
            0x0000_0020,
            "FILE_ATTRIBUTE_ARCHIVE is set when NTFS creates a file: {attrs:#x}"
        );
    }

    /// The one thing no test on this machine can build: a real cloud
    /// placeholder. Run it by hand once — point the variable at a file inside a
    /// OneDrive (or any Cloud Files engine's) folder you freed space for
    /// (right-click → "Free up space"):
    ///
    /// ```text
    /// RUST_TEST_THREADS=1 cargo test -p momotaro-ingest --lib -- --ignored a_real_placeholder
    /// ```
    ///
    /// It stops with the steps when the variable is unset, so "did not run"
    /// cannot read as "passed". The index-run half of the story is proven on
    /// the other side of the boundary — `momotaro-run` injects its own
    /// attributes through a seam that is `pub(crate)` over there, which is why
    /// this crate's readers were never able to reach it; what only a real
    /// sample can show is that the attribute word the reader sees carries the
    /// bit.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs a real OneDrive placeholder"]
    fn a_real_placeholder_is_recognized_by_the_reader() {
        let Some(sample) = std::env::var_os("MOMOTARO_ONEDRIVE_SAMPLE") else {
            panic!(
                "set MOMOTARO_ONEDRIVE_SAMPLE to a real cloud placeholder: sign in to \
                 OneDrive, turn on \"Files On-Demand\", right-click a file → \"Free up \
                 space\", and point this at it — then run this test again"
            );
        };
        let attrs = attributes_of(Path::new(&sample)).expect("read the sample's attributes");
        assert!(
            is_cloud_placeholder(attrs),
            "the sample must carry FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: attrs={attrs:#x}"
        );
    }

    #[test]
    fn an_icloud_stub_anchors_both_ends_of_the_name() {
        assert!(icloud_stub(".note.md.icloud"));
        assert!(
            icloud_stub(".icloud"),
            "a dot name that is exactly the suffix is still the shape"
        );
        // Both ends anchored: a substring or a suffix-only test would swallow
        // these ordinary names.
        assert!(
            !icloud_stub("x.icloud.md"),
            "a real note whose name contains .icloud"
        );
        assert!(!icloud_stub(".x.icloud.md"), "a hidden real note");
        assert!(
            !icloud_stub("note.md.icloud"),
            "no leading dot: not the stub shape"
        );
        assert!(!icloud_stub("notes.icloud"), "no leading dot");
        assert!(
            !icloud_stub(".x.ICLOUD"),
            "the system writes lowercase; the comparison is exact"
        );
        assert!(
            !icloud_stub(".fooicloud"),
            "the suffix needs its dot: `.icloud`, not `icloud`"
        );
        assert!(!icloud_stub("fooicloud"), "no leading dot either");
    }

    #[test]
    fn reserved_device_names_match_exactly_and_include_the_superscripts() {
        // 28 names: the four classic ones, COM/LPT 1–9, and the superscript
        // digits the attribute table's note treats as digits.
        assert_eq!(RESERVED_NAMES.len(), 28, "the official list, counted");
        for name in [
            "con.md",
            "CON.MD",
            "aux",
            "nul.txt",
            "com1.md",
            "lpt9",
            "com¹.md",
            "LPT³",
            "com²",
            // A device name with more than one dot: the segment *before the
            // first* dot decides, so `nul.tar.gz` is `NUL` with an extension.
            "nul.tar.gz",
            "com1.a.b",
        ] {
            assert_eq!(
                windows_incompatible_components(name),
                vec![name.to_string()],
                "{name:?} is reserved"
            );
        }
        // Prefix matching and whole-name comparison are both wrong: these are
        // legal ordinary names, measured on a Windows machine.
        for name in [
            "constitution.md",
            "console.md",
            "aux2.md",
            "com10.md",
            "com0.md",
            "lpt10.md",
            "conartist",
        ] {
            assert!(
                windows_incompatible_components(name).is_empty(),
                "{name:?} is legal on Windows"
            );
        }
    }

    #[test]
    fn reserved_characters_and_controls_are_flagged() {
        for name in [
            "a|b.md", "a*b.md", "a<b.md", "a>b.md", "a\"b.md", "a:b.md", "a\\b.md", "a?b.md",
        ] {
            assert!(
                !windows_incompatible_components(name).is_empty(),
                "{name:?} carries a reserved character"
            );
        }
        for name in ["a\0b.md", "a\u{1}b.md", "a\u{1f}b.md"] {
            assert!(
                !windows_incompatible_components(name).is_empty(),
                "{name:?} carries a control character"
            );
        }
        assert!(
            windows_incompatible_components("a\u{7f}b.md").is_empty(),
            "DEL is not in the documented 0–31 range"
        );
    }

    #[test]
    fn trailing_dots_and_spaces_are_flagged_but_only_at_the_end() {
        assert!(!windows_incompatible_components("foo.").is_empty());
        assert!(!windows_incompatible_components("foo ").is_empty());
        for name in ["trail .md", "sp ace.md", ".temp", ".file.md", "a.md"] {
            assert!(
                windows_incompatible_components(name).is_empty(),
                "{name:?} is fine: a leading dot and interior spaces are legal"
            );
        }
    }

    #[test]
    fn the_components_report_only_what_is_wrong() {
        assert_eq!(
            windows_incompatible_components("dir./a.md"),
            vec!["dir.".to_string()]
        );
        assert_eq!(
            windows_incompatible_components("ok/a|b.md/more.md"),
            vec!["a|b.md".to_string()]
        );
        assert!(windows_incompatible_components("ok/note.md").is_empty());
    }
}
