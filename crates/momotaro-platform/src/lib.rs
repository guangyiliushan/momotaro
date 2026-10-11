//! The platform reads that have no `std` answer.
//!
//! `std::os::windows` exposes no volume kind — no drive type, no filesystem
//! label — so the two questions this module asks of a path ("is this a
//! network volume?", "does this volume timestamp coarsely?") can only be
//! answered through the Win32 API, and an FFI call needs `unsafe`.
//!
//! **This is the one crate in the workspace without `#![forbid(unsafe_code)]`,
//! on purpose.** Every other member keeps its guarantee byte for byte; the
//! unsafe boundary is one crate, one file, so the answer to "where is the
//! unsafe in this project" is a crate name rather than a grep. A second FFI
//! call should come here — not start a second boundary.
//!
//! Nothing here reads file bytes. The calls take a path, ask the volume about
//! itself, and return; a placeholder's contents are never touched, which is
//! the same rule `momotaro_ingest::attributes_of` follows.
//!
//! # Examples
//!
//! Ask what the volume holding a path is like:
//!
//! ```
//! use momotaro_platform::{VolumeInfo, volume_info};
//! use std::path::Path;
//!
//! // `UNKNOWN` is "we could not tell", which is not the same answer as "no".
//! assert_eq!(VolumeInfo::UNKNOWN.remote, None);
//!
//! let info = volume_info(Path::new("."));
//! if info.remote == Some(true) {
//!     println!("this path sits on a network volume");
//! }
//! ```

#![warn(missing_docs)]
#![warn(rustdoc::all)]
#![warn(rustdoc::unescaped_backticks)]

use std::path::Path;

/// What the volume holding a path is like, as far as this build can tell.
///
/// Both fields are [`Option`] because "we could not tell" and "no" are
/// different answers and callers must not collapse them: a network volume we
/// failed to identify is not a local one, and a caller that read `None` as
/// `false` would stop warning on exactly the machines where the warning
/// matters. Callers branch on `Some(true)` for the positive claim and treat
/// `None` as "unreported", never as a denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VolumeInfo {
    /// `Some(true)` when the volume is a network volume.
    pub remote: Option<bool>,
    /// `Some(true)` when the volume timestamps coarsely enough that a matching
    /// stat cannot be trusted across a same-tick write (FAT/exFAT).
    pub coarse: Option<bool>,
}

impl VolumeInfo {
    /// What a platform with no volume API says: nothing, and not "no".
    pub const UNKNOWN: VolumeInfo = VolumeInfo {
        remote: None,
        coarse: None,
    };
}

/// What the volume holding `path` is, or [`VolumeInfo::UNKNOWN`] where the
/// platform has no such answer or the path has no volume we could resolve.
///
/// Windows only: everywhere else this costs no syscall and returns `UNKNOWN`,
/// which is the truthful answer — `std` offers no volume kind on any of the
/// platforms this project builds for.
///
/// # Examples
///
/// ```
/// use momotaro_platform::volume_info;
/// use std::path::Path;
///
/// // The answer is the volume's, so only match on it — never assume one.
/// let info = volume_info(Path::new("."));
/// match (info.remote, info.coarse) {
///     (Some(true), _) => println!("network volume: no real-time promise"),
///     (_, Some(true)) => println!("coarse timestamps: a matching stat proves nothing"),
///     _ => println!("nothing to warn about, or nothing this build could tell"),
/// }
/// ```
#[must_use]
pub fn volume_info(path: &Path) -> VolumeInfo {
    windows_volume_info(path)
}

/// The Windows implementation of [`volume_info`].
///
/// Two calls after resolving the volume root: the drive type says whether the
/// volume is remote, and the filesystem label says whether its timestamps are
/// coarse. Both failures degrade to "unknown" rather than to "no" — a volume
/// we cannot inspect is a volume we must not claim anything about.
#[cfg(windows)]
fn windows_volume_info(path: &Path) -> VolumeInfo {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    };

    /// WCHARs for a volume root: `\\?\UNC\server\share\` is the longest shape
    /// `GetVolumePathNameW` produces, and 320 leaves headroom over `MAX_PATH`.
    const ROOT_CHARS: usize = 320;
    /// `VOLUME_NAME_LEN` is 260; both name buffers are `MAX_PATH + 1` WCHARs.
    const NAME_CHARS: usize = 261;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut root = [0u16; ROOT_CHARS];
    // SAFETY: `wide` is NUL-terminated, `root` is a caller-owned buffer of
    // `root.len()` WCHARs as the API requires, and no pointer outlives this
    // call. A zero return means the path resolves to no volume; `UNKNOWN` is
    // the honest answer to that, not an error.
    let resolved =
        unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) };
    if resolved == 0 {
        return VolumeInfo::UNKNOWN;
    }

    // SAFETY: `root` is the NUL-terminated buffer `GetVolumePathNameW` just
    // filled in.
    let remote = remote_of(unsafe { GetDriveTypeW(root.as_ptr()) });

    let mut label = [0u16; NAME_CHARS];
    let mut serial = 0u32;
    let mut max_component = 0u32;
    let mut flags = 0u32;
    let mut fs_name = [0u16; NAME_CHARS];
    // SAFETY: same buffer discipline; every out-pointer is a live local of the
    // right type and the name buffers are `NAME_CHARS` WCHARs wide.
    let described = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    let coarse = if described == 0 {
        None
    } else {
        Some(is_coarse_filesystem(&wide_cstring(&fs_name)))
    };

    VolumeInfo { remote, coarse }
}

/// What `GetDriveTypeW`'s return says about remoteness.
///
/// The two failure values (`DRIVE_UNKNOWN`, `DRIVE_NO_ROOT_DIR`) answer
/// "unknown", not "no": the doc above promises both failures degrade to
/// unknown, and `== DRIVE_REMOTE` as the whole mapping broke that promise
/// for this call — an unreadable volume was reported as a local one. Every
/// known local kind answers `false`.
#[cfg(windows)]
fn remote_of(drive_type: u32) -> Option<bool> {
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_UNKNOWN,
    };
    match drive_type {
        DRIVE_REMOTE => Some(true),
        DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR => None,
        _ => Some(false),
    }
}

/// A Windows wide string cut at its first NUL — the terminator
/// `GetVolumeInformationW` writes, not a character of the name.
///
/// Split out because the cut is the whole reason the FAT relaxation is
/// reachable: reading to the first **non-zero** unit instead yields an empty
/// name, `is_coarse_filesystem("")` is `false`, and `coarse` is then
/// `Some(false)` for every volume — so `index_service`'s content-check path
/// never fires and a real change on a two-second filesystem is
/// skipped as "stat did not move". Nothing observed that: the only Windows
/// assertion checked `coarse.is_some()`, and `is_coarse_filesystem`'s own
/// tests take `&str` and never reach this slice.
#[cfg(any(windows, test))]
fn wide_cstring(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// The non-Windows answer to [`volume_info`]: nothing, and not "no".
///
/// `std` exposes no volume kind here either, and the alternatives (parsing
/// `/proc/mounts`, calling `statfs`) would be a second implementation of a
/// question only one caller asks. The FAT relaxation therefore does
/// not apply off Windows.
#[cfg(not(windows))]
fn windows_volume_info(_path: &Path) -> VolumeInfo {
    VolumeInfo::UNKNOWN
}

/// Whether a Windows filesystem label names a volume that timestamps coarsely.
///
/// FAT stores mtime with a two-second resolution, so a file written inside the
/// same tick as our write keeps the timestamp it had and a stat comparison
/// cannot tell the two apart — the case that goes to a content check.
/// Comparison is case-insensitive and whole-label: Windows reports `FAT`,
/// `FAT32` and `exFAT`, and nothing else in practice starts with `FAT`.
/// `pub` would only be hiding `dead_code` off Windows, where nothing calls
/// it: the one production caller is `windows_volume_info`, and the tests are
/// the other. The gate is local instead, so the crate's export surface says
/// what the crate actually offers.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))]
fn is_coarse_filesystem(label: &str) -> bool {
    let upper = label.trim().to_ascii_uppercase();
    upper == "EXFAT" || upper.starts_with("FAT")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_coarse_label_rule_names_the_fat_family_only() {
        // A whole-label spelling pinned by hand: `starts_with("FAT")` is what
        // the production predicate uses, so the interesting half of this test
        // is the ones it must NOT catch — a substring test would flag nothing
        // today and everything the moment a label like `FATBACKUP` appears.
        assert!(is_coarse_filesystem("FAT"));
        assert!(is_coarse_filesystem("FAT32"));
        assert!(is_coarse_filesystem("exFAT"));
        assert!(is_coarse_filesystem("ExFat"));
        assert!(is_coarse_filesystem(" FAT32 "));
        assert!(!is_coarse_filesystem("NTFS"));
        assert!(!is_coarse_filesystem("ReFS"));
        assert!(!is_coarse_filesystem("ext4"));
        assert!(!is_coarse_filesystem("APFS"));
        assert!(!is_coarse_filesystem(""));
        assert!(
            !is_coarse_filesystem("NOTFAT"),
            "a label that merely contains the family name is not of it — \
             the case the substring mutation must fail"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_drive_type_maps_its_failures_to_unknown() {
        // `DRIVE_UNKNOWN` and `DRIVE_NO_ROOT_DIR` are GetDriveTypeW's two
        // failure returns; `== DRIVE_REMOTE` as the whole mapping read them
        // as "local", and the crate doc promises the opposite.
        use windows_sys::Win32::System::WindowsProgramming::{
            DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_REMOVABLE,
            DRIVE_UNKNOWN,
        };
        assert_eq!(remote_of(DRIVE_REMOTE), Some(true));
        assert_eq!(remote_of(DRIVE_FIXED), Some(false));
        assert_eq!(remote_of(DRIVE_REMOVABLE), Some(false));
        assert_eq!(remote_of(DRIVE_CDROM), Some(false));
        assert_eq!(
            remote_of(DRIVE_UNKNOWN),
            None,
            "a drive type we could not read is not a local one"
        );
        assert_eq!(remote_of(DRIVE_NO_ROOT_DIR), None);
    }

    #[test]
    fn an_unknown_volume_claims_nothing() {
        // `None` is "we could not tell". A caller that read it as `false`
        // would stop warning on the machines the warning exists for, so the
        // constant's shape is part of the contract, not a default.
        assert_eq!(VolumeInfo::UNKNOWN.remote, None);
        assert_eq!(VolumeInfo::UNKNOWN.coarse, None);
        assert_ne!(VolumeInfo::UNKNOWN.remote, Some(false));
    }

    /// The non-Windows arm is `UNKNOWN`, not `Some(false)` — the same claim
    /// the constant makes, checked against a real path so a future `cfg` slip
    /// that returns "local" here goes red.
    #[cfg(not(windows))]
    #[test]
    fn off_windows_a_path_claims_nothing() {
        assert_eq!(volume_info(Path::new("/")), VolumeInfo::UNKNOWN);
    }

    /// On Windows a resolvable local path answers `Some(_)` for both fields —
    /// it is not `UNKNOWN`. This asserts the calls happen, not what they say:
    /// the verdict depends on the machine, so the value itself is never
    /// pinned here. What *is* pinned is that `doctor` and `watch` surface it
    /// (`network_volume` on the health report, `network_volume` on a watch
    /// line) and that both render the same sentence.
    /// The cutter, fed a synthetic UTF-16 buffer instead of a live volume —
    /// the only way to tell "first NUL" from "first non-zero" without a disk
    /// whose label happens to start with a NUL.
    #[test]
    fn the_volume_name_is_cut_at_the_first_nul_and_nowhere_else() {
        let wide = |text: &str, pad: Vec<u16>| {
            let mut units: Vec<u16> = text.encode_utf16().collect();
            units.extend(pad);
            units
        };

        assert_eq!(
            wide_cstring(&wide("FAT", vec![0, 0x58])),
            "FAT",
            "the terminator ends the name; whatever follows it is not part of it"
        );
        assert_eq!(
            wide_cstring(&wide("NTFS", vec![0, 0x58])),
            "NTFS",
            "…so a non-FAT label reads back whole, and `is_coarse_filesystem` \
             says no — which is what the content check depends on"
        );
        assert_eq!(
            wide_cstring(&wide("exFAT", Vec::new())),
            "exFAT",
            "with no terminator the whole buffer is the name"
        );
        assert_eq!(
            wide_cstring(&[]),
            "",
            "and an empty buffer is an empty name"
        );

        // The half that the mutants actually change: cutting at the first
        // non-zero unit gives "" for every label, and `""` is not coarse.
        assert!(
            is_coarse_filesystem("FAT"),
            "…so a FAT label has to survive the cut as `FAT`, not as empty"
        );
        assert!(
            !is_coarse_filesystem(&wide_cstring(&wide("NTFS", vec![0, 0x58]))),
            "…and NTFS has to survive as NTFS"
        );
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_a_resolvable_path_answers_both_questions() {
        let info = volume_info(Path::new("."));
        assert_eq!(info.remote, Some(false), "the repo is on a local volume");
        assert!(info.coarse.is_some(), "the filesystem label was read");
    }
}
