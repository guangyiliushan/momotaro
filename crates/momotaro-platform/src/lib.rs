//! The platform reads that have no `std` answer.
//!
//! `std::os::windows` exposes no volume kind — no drive type, no filesystem
//! label — so the two questions D31 and D40 §4.3 ask of a path ("is this a
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

#![warn(missing_docs)]

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
    /// `Some(true)` when the volume is a network volume (D31, D40 §4.8).
    pub remote: Option<bool>,
    /// `Some(true)` when the volume timestamps coarsely enough that a matching
    /// stat cannot be trusted across a same-tick write (D40 §4.3, FAT/exFAT).
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
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_REMOTE;

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
    let remote = unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE };

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
        let end = fs_name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(fs_name.len());
        Some(is_coarse_filesystem(&String::from_utf16_lossy(
            &fs_name[..end],
        )))
    };

    VolumeInfo {
        remote: Some(remote),
        coarse,
    }
}

/// The non-Windows answer to [`volume_info`]: nothing, and not "no".
///
/// `std` exposes no volume kind here either, and the alternatives (parsing
/// `/proc/mounts`, calling `statfs`) would be a second implementation of a
/// question only one caller asks. D40 §4.3's FAT relaxation therefore does
/// not apply off Windows; the contract says so in `ingest-interop` §4 第 3 条.
#[cfg(not(windows))]
fn windows_volume_info(_path: &Path) -> VolumeInfo {
    VolumeInfo::UNKNOWN
}

/// Whether a Windows filesystem label names a volume that timestamps coarsely.
///
/// FAT stores mtime with a two-second resolution, so a file written inside the
/// same tick as our write keeps the timestamp it had and a stat comparison
/// cannot tell the two apart — the case D40 §4.3 sends to a content check.
/// Comparison is case-insensitive and whole-label: Windows reports `FAT`,
/// `FAT32` and `exFAT`, and nothing else in practice starts with `FAT`.
#[must_use]
pub fn is_coarse_filesystem(label: &str) -> bool {
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
    #[cfg(windows)]
    #[test]
    fn on_windows_a_resolvable_path_answers_both_questions() {
        let info = volume_info(Path::new("."));
        assert_eq!(info.remote, Some(false), "the repo is on a local volume");
        assert!(info.coarse.is_some(), "the filesystem label was read");
    }
}
