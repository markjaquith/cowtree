use std::{
    collections::{HashMap, HashSet},
    ffi::{CString, OsStr, OsString},
    fs, io,
    mem::MaybeUninit,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use crate::error::{Error, Result};

const CLONE_NOFOLLOW: libc::c_int = 0x0001;
const TEMPORARY_MARKER: &str = ".cowtree-clone.";

unsafe extern "C" {
    fn clonefile(
        src: *const libc::c_char,
        dst: *const libc::c_char,
        flags: libc::c_int,
    ) -> libc::c_int;
}

#[derive(Debug, PartialEq, Eq)]
pub enum CloneOutcome {
    Cloned,
    ChangedDuringClone,
    NotRegular,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl Identity {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

pub fn validate_filesystem(source: &Path, target: &Path) -> Result<()> {
    let source_metadata = fs::metadata(source)?;
    let target_metadata = fs::metadata(target)?;
    if source_metadata.dev() != target_metadata.dev() {
        return Err(Error::CrossVolume);
    }
    let filesystem = filesystem_type(source)?;
    if filesystem != "apfs" {
        return Err(Error::UnsupportedFilesystem(filesystem));
    }
    Ok(())
}

pub fn cleanup_stale_clones(
    target: &Path,
    tracked: &[PathBuf],
    untracked: &[PathBuf],
) -> Result<u64> {
    let tracked: HashSet<_> = tracked.iter().cloned().collect();
    let untracked: HashSet<_> = untracked.iter().cloned().collect();
    let mut directories: HashMap<PathBuf, HashSet<OsString>> = HashMap::new();
    for relative in &tracked {
        let Some(name) = relative.file_name() else {
            continue;
        };
        let original = target.join(relative);
        if !fs::symlink_metadata(&original).is_ok_and(|meta| meta.file_type().is_file()) {
            continue;
        }
        let parent = relative.parent().unwrap_or(Path::new(""));
        if !safe_directory(target, parent) {
            continue;
        }
        directories
            .entry(parent.to_owned())
            .or_default()
            .insert(name.to_owned());
    }

    let mut removed = 0;
    for (relative_directory, originals) in directories {
        let directory = target.join(&relative_directory);
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let bytes = name.as_bytes();
            let Some(pid) = stale_clone_pid(bytes, &originals) else {
                continue;
            };
            let relative = relative_directory.join(&name);
            if !untracked.contains(&relative) || process_exists(pid) {
                continue;
            }
            let Ok(before) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !before.file_type().is_file() {
                continue;
            }
            let still_same = fs::symlink_metadata(entry.path()).is_ok_and(|after| {
                after.file_type().is_file()
                    && after.dev() == before.dev()
                    && after.ino() == before.ino()
            });
            if still_same && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn safe_directory(root: &Path, relative: &Path) -> bool {
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        if !fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_dir()) {
            return false;
        }
    }
    true
}

fn stale_clone_pid(name: &[u8], originals: &HashSet<OsString>) -> Option<u32> {
    let marker = TEMPORARY_MARKER.as_bytes();
    let marker_at =
        name.windows(marker.len())
            .enumerate()
            .rev()
            .find_map(|(index, candidate)| {
                (candidate == marker && originals.contains(OsStr::from_bytes(&name[..index])))
                    .then_some(index)
            })?;
    let suffix = &name[marker_at + marker.len()..];
    let mut parts = suffix.split(|byte| *byte == b'.');
    let pid = parse_decimal(parts.next()?)?;
    let _sequence = u64::try_from(parse_decimal(parts.next()?)?).ok()?;
    let nonce = parse_decimal(parts.next()?)?;
    if parts.next().is_some() || pid == 0 || nonce == 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    (nonce <= now).then_some(u32::try_from(pid).ok()?)
}

fn parse_decimal(bytes: &[u8]) -> Option<u128> {
    if bytes.is_empty() || !bytes.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    (unsafe { libc::kill(pid, 0) }) == 0
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn filesystem_type(path: &Path) -> Result<String> {
    let path = cstring(path)?;
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    let bytes: Vec<u8> = stat
        .f_fstypename
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn clone_replacing(
    source_root: &Path,
    target_root: &Path,
    relative: &Path,
    sequence: u64,
) -> Result<CloneOutcome> {
    let source = source_root.join(relative);
    let target = target_root.join(relative);
    let source_before = match fs::symlink_metadata(&source) {
        Ok(value) => value,
        Err(_) => return Ok(CloneOutcome::NotRegular),
    };
    let target_before = match fs::symlink_metadata(&target) {
        Ok(value) => value,
        Err(_) => return Ok(CloneOutcome::NotRegular),
    };
    if !source_before.file_type().is_file() || !target_before.file_type().is_file() {
        return Ok(CloneOutcome::NotRegular);
    }

    let temporary = temporary_path(&target, sequence);
    let source_c = cstring(&source)?;
    let temporary_c = cstring(&temporary)?;
    if unsafe { clonefile(source_c.as_ptr(), temporary_c.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        return Err(Error::Clone {
            path: relative.to_owned(),
            source: io::Error::last_os_error(),
        });
    }

    let operation = (|| -> Result<CloneOutcome> {
        let source_after = fs::symlink_metadata(&source)?;
        if Identity::from(&source_before) != Identity::from(&source_after) {
            return Ok(CloneOutcome::ChangedDuringClone);
        }
        // clonefile preserves the source mode except for setuid/setgid. Most
        // worktree files already have the desired mode, so avoid an extra
        // metadata write for every file in a large checkout.
        let target_mode = target_before.mode() & 0o7777;
        if source_before.mode() & 0o1777 != target_mode {
            fs::set_permissions(&temporary, fs::Permissions::from_mode(target_mode))?;
        }
        set_times(&temporary, &target_before)?;
        let target_after = fs::symlink_metadata(&target)?;
        if Identity::from(&target_before) != Identity::from(&target_after) {
            return Ok(CloneOutcome::ChangedDuringClone);
        }
        fs::rename(&temporary, &target)?;
        Ok(CloneOutcome::Cloned)
    })();
    if !matches!(operation, Ok(CloneOutcome::Cloned)) {
        let _ = fs::remove_file(&temporary);
    }
    operation
}

fn temporary_path(target: &Path, sequence: u64) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().as_bytes().to_vec();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    name.extend_from_slice(
        format!(
            "{}{}.{}.{}",
            TEMPORARY_MARKER,
            std::process::id(),
            sequence,
            nonce
        )
        .as_bytes(),
    );
    target.with_file_name(std::ffi::OsStr::from_bytes(&name))
}

fn set_times(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    let path = cstring(path)?;
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    if unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Message(format!("path contains NUL: {path:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_stale_untracked_clone_siblings() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path();
        fs::write(target.join("file"), "original").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let stale = target.join(format!("file{TEMPORARY_MARKER}{}.0.{now}", i32::MAX));
        let active = target.join(format!(
            "file{TEMPORARY_MARKER}{}.1.{now}",
            std::process::id()
        ));
        let malformed = target.join(format!("file{TEMPORARY_MARKER}dead.2.{now}"));
        let tracked_clone = target.join(format!("file{TEMPORARY_MARKER}{}.3.{now}", i32::MAX));
        for path in [&stale, &active, &malformed, &tracked_clone] {
            fs::write(path, "clone").unwrap();
        }

        let tracked = vec![
            PathBuf::from("file"),
            tracked_clone.strip_prefix(target).unwrap().to_owned(),
        ];
        let untracked = vec![
            stale.strip_prefix(target).unwrap().to_owned(),
            active.strip_prefix(target).unwrap().to_owned(),
            malformed.strip_prefix(target).unwrap().to_owned(),
        ];
        assert_eq!(
            cleanup_stale_clones(target, &tracked, &untracked).unwrap(),
            1
        );
        assert!(!stale.exists());
        assert!(active.exists());
        assert!(malformed.exists());
        assert!(tracked_clone.exists());
    }
}
