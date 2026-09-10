use std::{
    ffi::CString,
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
            ".cowtree-clone.{}.{}.{}",
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
