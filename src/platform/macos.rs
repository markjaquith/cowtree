use std::{
    cell::RefCell,
    ffi::{CStr, CString},
    fs, io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

use crate::error::{Error, Result};

const CLONE_NOFOLLOW: libc::c_int = 0x0001;

unsafe extern "C" {
    fn clonefileat(
        src_dirfd: libc::c_int,
        src: *const libc::c_char,
        dst_dirfd: libc::c_int,
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
    fn from_stat(stat: &libc::stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
            size: stat.st_size as u64,
            modified_seconds: stat.st_mtime,
            modified_nanoseconds: stat.st_mtime_nsec,
            changed_seconds: stat.st_ctime,
            changed_nanoseconds: stat.st_ctime_nsec,
        }
    }
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

struct Parents {
    source_path: PathBuf,
    target_path: PathBuf,
    source: fs::File,
    target: fs::File,
}

thread_local! {
    // One pair per clone worker, not an unbounded cache or a shared lock.
    static PARENTS: RefCell<Option<Parents>> = const { RefCell::new(None) };
}

fn open_directory(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn stat_at(directory: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = MaybeUninit::uninit();
    if unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

pub fn clone_replacing(
    source_root: &Path,
    target_root: &Path,
    relative: &Path,
    sequence: u64,
) -> Result<CloneOutcome> {
    let source = source_root.join(relative);
    let target = target_root.join(relative);
    let source_parent = source
        .parent()
        .ok_or_else(|| Error::UnsafePath(relative.to_owned()))?;
    let target_parent = target
        .parent()
        .ok_or_else(|| Error::UnsafePath(relative.to_owned()))?;
    PARENTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.as_ref().is_none_or(|parents| {
            parents.source_path != source_parent || parents.target_path != target_parent
        }) {
            // Drop old descriptors before opening the next pair.
            *cache = None;
            let (Ok(source_directory), Ok(target_directory)) =
                (open_directory(source_parent), open_directory(target_parent))
            else {
                return Ok(CloneOutcome::NotRegular);
            };
            *cache = Some(Parents {
                source_path: source_parent.to_owned(),
                target_path: target_parent.to_owned(),
                source: source_directory,
                target: target_directory,
            });
        }
        let result = clone_in_parents(
            cache.as_ref().unwrap(),
            &source,
            &target,
            relative,
            sequence,
        );
        if !matches!(result, Ok(CloneOutcome::Cloned)) {
            *cache = None;
        }
        result
    })
}

fn clone_in_parents(
    parents: &Parents,
    source: &Path,
    target: &Path,
    relative: &Path,
    sequence: u64,
) -> Result<CloneOutcome> {
    let name = cstring(Path::new(
        relative
            .file_name()
            .ok_or_else(|| Error::UnsafePath(relative.to_owned()))?,
    ))?;
    let source_fd = parents.source.as_raw_fd();
    let target_fd = parents.target.as_raw_fd();
    let source_before = match stat_at(source_fd, &name) {
        Ok(value) => value,
        Err(_) => return Ok(CloneOutcome::NotRegular),
    };
    let target_before = match stat_at(target_fd, &name) {
        Ok(value) => value,
        Err(_) => return Ok(CloneOutcome::NotRegular),
    };
    if source_before.st_mode & libc::S_IFMT != libc::S_IFREG
        || target_before.st_mode & libc::S_IFMT != libc::S_IFREG
    {
        return Ok(CloneOutcome::NotRegular);
    }

    let temporary = temporary_path(Path::new(relative.file_name().unwrap()), sequence);
    let temporary_c = cstring(&temporary)?;
    if unsafe {
        clonefileat(
            source_fd,
            name.as_ptr(),
            target_fd,
            temporary_c.as_ptr(),
            CLONE_NOFOLLOW,
        )
    } != 0
    {
        return Err(Error::Clone {
            path: relative.to_owned(),
            source: io::Error::last_os_error(),
        });
    }

    let operation = (|| -> Result<CloneOutcome> {
        // Resolve the original full paths for the final checks. Checking only
        // through cached descriptors would miss replaced/renamed parents.
        let source_after = fs::symlink_metadata(source)?;
        if Identity::from_stat(&source_before) != Identity::from(&source_after) {
            return Ok(CloneOutcome::ChangedDuringClone);
        }
        // clonefile preserves the source mode except for setuid/setgid. Most
        // worktree files already have the desired mode, so avoid an extra
        // metadata write for every file in a large checkout.
        let target_mode = target_before.st_mode & 0o7777;
        if source_before.st_mode & 0o1777 != target_mode
            && unsafe { libc::fchmodat(target_fd, temporary_c.as_ptr(), target_mode, 0) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        set_times(target_fd, &temporary_c, &target_before)?;
        let target_after = fs::symlink_metadata(target)?;
        if Identity::from_stat(&target_before) != Identity::from(&target_after) {
            return Ok(CloneOutcome::ChangedDuringClone);
        }
        if unsafe { libc::renameat(target_fd, temporary_c.as_ptr(), target_fd, name.as_ptr()) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(CloneOutcome::Cloned)
    })();
    if !matches!(operation, Ok(CloneOutcome::Cloned)) {
        unsafe {
            libc::unlinkat(target_fd, temporary_c.as_ptr(), 0);
        }
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

fn set_times(directory: RawFd, path: &CStr, metadata: &libc::stat) -> Result<()> {
    let times = [
        libc::timespec {
            tv_sec: metadata.st_atime,
            tv_nsec: metadata.st_atime_nsec,
        },
        libc::timespec {
            tv_sec: metadata.st_mtime,
            tv_nsec: metadata.st_mtime_nsec,
        },
    ];
    if unsafe { libc::utimensat(directory, path.as_ptr(), times.as_ptr(), 0) } != 0 {
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
    fn cached_directory_replacement_does_not_replace_files() {
        let root = tempfile::tempdir().unwrap();
        if filesystem_type(root.path()).unwrap() != "apfs" {
            return;
        }
        for replace_source in [true, false] {
            let case = root.path().join(if replace_source {
                "source-case"
            } else {
                "target-case"
            });
            let source = case.join("source");
            let target = case.join("target");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(&target).unwrap();
            fs::write(source.join("file"), "original").unwrap();
            fs::write(target.join("file"), "original").unwrap();
            let parents = Parents {
                source_path: source.clone(),
                target_path: target.clone(),
                source: open_directory(&source).unwrap(),
                target: open_directory(&target).unwrap(),
            };
            let changed = if replace_source { &source } else { &target };
            let moved = case.join("moved");
            fs::rename(changed, &moved).unwrap();
            fs::create_dir(changed).unwrap();
            fs::write(changed.join("file"), "replacement").unwrap();
            let expected = fs::read(target.join("file")).unwrap();
            assert_eq!(
                clone_in_parents(
                    &parents,
                    &source.join("file"),
                    &target.join("file"),
                    Path::new("file"),
                    0
                )
                .unwrap(),
                CloneOutcome::ChangedDuringClone
            );
            assert_eq!(fs::read(target.join("file")).unwrap(), expected);
            assert_eq!(fs::read(moved.join("file")).unwrap(), b"original");
            assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
            assert_eq!(fs::read_dir(&moved).unwrap().count(), 1);
        }
    }

    #[test]
    fn cached_cloning_preserves_unusual_names_and_skips_symlinks() {
        let root = tempfile::tempdir().unwrap();
        if filesystem_type(root.path()).unwrap() != "apfs" {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        for (sequence, bytes) in [
            b"space name".as_slice(),
            b"line\nname",
            "unicode-☃".as_bytes(),
        ]
        .iter()
        .enumerate()
        {
            let name = Path::new(std::ffi::OsStr::from_bytes(bytes));
            fs::write(source.join(name), "contents").unwrap();
            fs::write(target.join(name), "contents").unwrap();
            assert_eq!(
                clone_replacing(&source, &target, name, sequence as u64).unwrap(),
                CloneOutcome::Cloned
            );
            assert_eq!(fs::read(target.join(name)).unwrap(), b"contents");
        }
        std::os::unix::fs::symlink("space name", target.join("link")).unwrap();
        fs::write(source.join("link"), "contents").unwrap();
        assert_eq!(
            clone_replacing(&source, &target, Path::new("link"), 3).unwrap(),
            CloneOutcome::NotRegular
        );
        assert!(
            fs::symlink_metadata(target.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
