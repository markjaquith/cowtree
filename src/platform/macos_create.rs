//! Clone into an absent path without borrowing compaction's replacement logic.
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path},
};

use crate::{
    eligibility::validate_relative,
    error::{Error, Result},
    platform::CloneOutcome,
};
use std::os::macos::fs::MetadataExt as _;

unsafe extern "C" {
    fn fclonefileat(
        source: libc::c_int,
        destination: libc::c_int,
        name: *const libc::c_char,
        flags: libc::c_int,
    ) -> libc::c_int;
}

fn name(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::UnsafePath(path.to_owned()))
}

/// Walk each component relative to an open directory, never following a link.
fn parent(root: &Path, relative: &Path, create: bool) -> Result<File> {
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    for component in relative.parent().unwrap_or(Path::new("")).components() {
        if component == Component::CurDir {
            continue;
        }
        let part = name(Path::new(component.as_os_str()))?;
        if create && unsafe { libc::mkdirat(directory.as_raw_fd(), part.as_ptr(), 0o777) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::UnsafePath(relative.to_owned()));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

fn identity(meta: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

pub fn clone_new(
    source: &Path,
    target: &Path,
    relative: &Path,
    mode: u32,
    verify: impl FnOnce(&mut File) -> Result<bool>,
) -> Result<CloneOutcome> {
    validate_relative(relative)?;
    let source_parent = match parent(source, relative, false) {
        Ok(parent) => parent,
        Err(_) => return Ok(CloneOutcome::NotRegular),
    };
    let basename = name(Path::new(
        relative
            .file_name()
            .ok_or_else(|| Error::UnsafePath(relative.to_owned()))?,
    ))?;
    let fd = unsafe {
        libc::openat(
            source_parent.as_raw_fd(),
            basename.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Ok(CloneOutcome::NotRegular);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let before = file.metadata()?;
    // clonefile copies xattrs and file flags. Such files go through Git rather
    // than carrying source-only metadata into a fresh checkout.
    if !before.is_file() || before.st_flags() != 0 || !ordinary_attributes(fd) {
        return Ok(CloneOutcome::NotRegular);
    }
    if !verify(&mut file)? {
        return Ok(CloneOutcome::NotRegular);
    }
    if identity(&before) != identity(&file.metadata()?) {
        return Ok(CloneOutcome::ChangedDuringClone);
    }
    let destination = parent(target, relative, true)?;
    // CLONE_NOOWNERCOPY; without CLONE_ACL the destination inherits its own
    // directory's ACL, just as open(O_CREAT) would.
    if unsafe { fclonefileat(fd, destination.as_raw_fd(), basename.as_ptr(), 2) } != 0 {
        return Err(Error::Clone {
            path: relative.to_owned(),
            source: io::Error::last_os_error(),
        });
    }
    let destination_fd = unsafe {
        libc::openat(
            destination.as_raw_fd(),
            basename.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if destination_fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let cloned = unsafe { File::from_raw_fd(destination_fd) };
    let cloned_identity = identity(&cloned.metadata()?);
    let result = (|| {
        if identity(&before) != identity(&file.metadata()?)
            || !fs::symlink_metadata(source.join(relative))
                .is_ok_and(|after| identity(&before) == identity(&after))
        {
            return Ok(CloneOutcome::ChangedDuringClone);
        }
        // Final root/path resolution detects renamed ancestors, not just edits
        // through the pinned source descriptor.
        if !fs::symlink_metadata(target.join(relative))
            .is_ok_and(|meta| identity(&meta) == cloned_identity)
        {
            return Err(Error::UnsafePath(relative.to_owned()));
        }
        if unsafe { libc::fchmod(destination_fd, mode as libc::mode_t) } != 0
            || unsafe { libc::futimens(destination_fd, std::ptr::null()) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(CloneOutcome::Cloned)
    })();
    if !matches!(result, Ok(CloneOutcome::Cloned)) {
        // Do not delete a replacement installed by another process. There is
        // still a final name-check/unlink race, as with compaction's rename.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                destination.as_raw_fd(),
                basename.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            let stat = unsafe { stat.assume_init() };
            if stat.st_dev as u64 == cloned_identity.0 && stat.st_ino == cloned_identity.1 {
                unsafe {
                    libc::unlinkat(destination.as_raw_fd(), basename.as_ptr(), 0);
                }
            }
        }
    }
    result
}

fn ordinary_attributes(fd: libc::c_int) -> bool {
    let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
    if size < 0 {
        return false;
    }
    let mut names = vec![0u8; size as usize];
    let read = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len(), 0) };
    if read != size {
        return false;
    }
    // macOS attaches this protected, system-managed attribute to ordinary new
    // files too. User xattrs/resource forks are never inherited from a source.
    names
        .split(|b| *b == 0)
        .all(|name| name.is_empty() || name == b"com.apple.provenance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{ClonePlatform, SystemPlatform};

    #[test]
    fn source_race_and_existing_destination_do_not_overwrite() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("file"), "before").unwrap();
        let result = clone_new(&source, &target, Path::new("file"), 0o644, |_| {
            fs::write(source.join("file"), "modified during verification")?;
            Ok(true)
        })
        .unwrap();
        assert_eq!(result, CloneOutcome::ChangedDuringClone);
        assert!(!target.join("file").exists());
        fs::write(target.join("file"), "external").unwrap();
        assert!(clone_new(&source, &target, Path::new("file"), 0o644, |_| Ok(true)).is_err());
        assert_eq!(fs::read(target.join("file")).unwrap(), b"external");
    }

    #[test]
    fn destination_symlink_ancestor_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        let outside = root.path().join("outside");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(source.join("dir/file"), "data").unwrap();
        std::os::unix::fs::symlink(&outside, target.join("dir")).unwrap();
        assert!(clone_new(&source, &target, Path::new("dir/file"), 0o644, |_| Ok(true)).is_err());
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }
}
