//! Clone into an absent path without borrowing compaction's replacement logic.
use std::{
    collections::{HashMap, HashSet},
    ffi::{CString, OsString},
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use crate::{
    eligibility::validate_relative,
    error::{Error, Result},
    platform::{CloneOutcome, ClonePhase, CloneTimings, ClonedDirectory},
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

/// Walk an existing relative directory without following links. Creation owns
/// directory setup; workers must not recreate a parent that disappeared.
fn open_parent(root: &Path, relative: &Path) -> Result<File> {
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    for component in relative.components() {
        if component == Component::CurDir {
            continue;
        }
        let part = name(Path::new(component.as_os_str()))?;
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

struct ParentCache {
    root: PathBuf,
    relative: PathBuf,
    directory: Option<File>,
}

impl ParentCache {
    fn new() -> Self {
        Self {
            root: PathBuf::new(),
            relative: PathBuf::new(),
            directory: None,
        }
    }

    fn get(&mut self, root: &Path, relative: &Path) -> Result<&File> {
        let relative = relative.parent().unwrap_or(Path::new(""));
        if self.directory.is_none() || self.root != root || self.relative != relative {
            self.directory = Some(open_parent(root, relative)?);
            self.root.clear();
            self.root.push(root);
            self.relative.clear();
            self.relative.push(relative);
        }
        Ok(self.directory.as_ref().unwrap())
    }
}

pub struct CloneDirectoryCache {
    source: ParentCache,
    target: ParentCache,
}

impl CloneDirectoryCache {
    pub fn new() -> Self {
        Self {
            source: ParentCache::new(),
            target: ParentCache::new(),
        }
    }
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
    directories: &mut CloneDirectoryCache,
    timings: Option<&CloneTimings>,
    verify: impl FnOnce(&mut File) -> Result<bool>,
) -> Result<CloneOutcome> {
    validate_relative(relative)?;
    let phase = start(timings);
    let source_parent = match directories.source.get(source, relative) {
        Ok(parent) => parent,
        Err(_) => {
            finish(timings, ClonePhase::SourceLookup, phase);
            return Ok(CloneOutcome::NotRegular);
        }
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
    finish(timings, ClonePhase::SourceLookup, phase);
    if fd < 0 {
        return Ok(CloneOutcome::NotRegular);
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let phase = start(timings);
    let before = file.metadata()?;
    // clonefile copies xattrs and file flags. Such files go through Git rather
    // than carrying source-only metadata into a fresh checkout.
    let ordinary = ordinary_attributes(fd);
    finish(timings, ClonePhase::SourceMetadata, phase);
    if !before.is_file() || before.st_flags() != 0 || !ordinary {
        return Ok(CloneOutcome::NotRegular);
    }
    let phase = start(timings);
    let verified = verify(&mut file);
    finish(timings, ClonePhase::DonorVerification, phase);
    if !verified? {
        return Ok(CloneOutcome::NotRegular);
    }
    let phase = start(timings);
    let stable = identity(&before) == identity(&file.metadata()?);
    finish(timings, ClonePhase::SourceStability, phase);
    if !stable {
        return Ok(CloneOutcome::ChangedDuringClone);
    }
    let phase = start(timings);
    let destination = directories.target.get(target, relative)?;
    finish(timings, ClonePhase::TargetLookup, phase);
    // CLONE_NOOWNERCOPY; without CLONE_ACL the destination inherits its own
    // directory's ACL, just as open(O_CREAT) would.
    let phase = start(timings);
    let cloned = unsafe { fclonefileat(fd, destination.as_raw_fd(), basename.as_ptr(), 2) };
    let clone_error = (cloned != 0).then(io::Error::last_os_error);
    finish(timings, ClonePhase::CloneFile, phase);
    if let Some(source) = clone_error {
        return Err(Error::Clone {
            path: relative.to_owned(),
            source,
        });
    }
    let phase = start(timings);
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
    finish(timings, ClonePhase::DestinationLookup, phase);
    let phase = start(timings);
    let mut result = (|| {
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
        Ok(CloneOutcome::Cloned)
    })();
    finish(timings, ClonePhase::RaceValidation, phase);
    if matches!(result, Ok(CloneOutcome::Cloned)) {
        let phase = start(timings);
        if unsafe { libc::fchmod(destination_fd, mode as libc::mode_t) } != 0
            || unsafe { libc::futimens(destination_fd, std::ptr::null()) } != 0
        {
            result = Err(io::Error::last_os_error().into());
        }
        finish(timings, ClonePhase::MetadataFinalization, phase);
    }
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

pub fn clone_directory_new(
    source: &Path,
    target: &Path,
    relative: &Path,
    entries: &[&crate::git::TreeEntry],
    mask: u32,
    sequence: u64,
    cancelled: &AtomicUsize,
) -> Result<Option<ClonedDirectory>> {
    validate_relative(relative)?;
    let parent = relative.parent().unwrap_or(Path::new(""));
    let source_parent = match open_parent(source, parent) {
        Ok(parent) => parent,
        Err(_) => return Ok(None),
    };
    let basename = name(Path::new(
        relative
            .file_name()
            .ok_or_else(|| Error::UnsafePath(relative.to_owned()))?,
    ))?;
    let source_fd = unsafe {
        libc::openat(
            source_parent.as_raw_fd(),
            basename.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if source_fd < 0 {
        return Ok(None);
    }
    let source_directory = unsafe { File::from_raw_fd(source_fd) };
    let source_meta = source_directory.metadata()?;
    if !source_meta.is_dir() || source_meta.st_flags() != 0 || !ordinary_attributes(source_fd) {
        return Ok(None);
    }

    let target_parent = open_parent(target, parent)?;
    if !directory_path_matches(&target_parent, &target.join(parent)) {
        return Err(Error::UnsafePath(relative.to_owned()));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary_name = OsString::from(format!(
        ".cowtree-directory.{}.{}.{}",
        std::process::id(),
        sequence,
        nonce
    ));
    let temporary_c = name(Path::new(&temporary_name))?;
    let cloned = unsafe {
        fclonefileat(
            source_fd,
            target_parent.as_raw_fd(),
            temporary_c.as_ptr(),
            2, // CLONE_NOOWNERCOPY; inherit destination ACLs.
        )
    };
    if cloned != 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => Ok(None),
            _ => Err(Error::Clone {
                path: relative.to_owned(),
                source: error,
            }),
        };
    }

    let temporary = target.join(parent).join(&temporary_name);
    let temporary_meta = fs::symlink_metadata(&temporary)?;
    if !temporary_meta.file_type().is_dir() {
        return Err(Error::UnsafePath(relative.to_owned()));
    }
    let temporary_identity = (temporary_meta.dev(), temporary_meta.ino());
    let validation = validate_directory_clone(&temporary, relative, entries, mask, cancelled);
    let validated = match validation {
        Ok(Some(value)) => value,
        Ok(None) => {
            remove_private_clone(&temporary, temporary_identity)?;
            return Ok(None);
        }
        Err(error) => {
            remove_private_clone(&temporary, temporary_identity)?;
            return Err(error);
        }
    };

    if !directory_path_matches(&target_parent, &target.join(parent)) {
        // The private clone is now reachable only through the displaced parent
        // descriptor. Preserve it and the registered worktree for inspection;
        // never remove a replacement found at the original path.
        return Err(Error::UnsafePath(relative.to_owned()));
    }

    if unsafe {
        libc::renameatx_np(
            target_parent.as_raw_fd(),
            temporary_c.as_ptr(),
            target_parent.as_raw_fd(),
            basename.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        remove_private_clone(&temporary, temporary_identity)?;
        return Err(Error::Clone {
            path: relative.to_owned(),
            source: error,
        });
    }
    if !fs::symlink_metadata(target.join(relative)).is_ok_and(|metadata| {
        metadata.file_type().is_dir() && (metadata.dev(), metadata.ino()) == temporary_identity
    }) {
        return Err(Error::UnsafePath(relative.to_owned()));
    }
    Ok(Some(validated))
}

fn directory_path_matches(directory: &File, path: &Path) -> bool {
    let Ok(opened) = directory.metadata() else {
        return false;
    };
    fs::symlink_metadata(path).is_ok_and(|named| {
        named.file_type().is_dir() && (named.dev(), named.ino()) == (opened.dev(), opened.ino())
    })
}

fn remove_private_clone(path: &Path, expected: (u64, u64)) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() || (metadata.dev(), metadata.ino()) != expected {
        return Err(Error::UnsafePath(path.to_owned()));
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_directory_clone(
    root: &Path,
    final_root: &Path,
    entries: &[&crate::git::TreeEntry],
    mask: u32,
    cancelled: &AtomicUsize,
) -> Result<Option<ClonedDirectory>> {
    let mut files = HashMap::new();
    let mut directories = HashSet::from([PathBuf::new()]);
    for entry in entries {
        if !entry.regular {
            return Ok(None);
        }
        let Ok(relative) = entry.path.strip_prefix(final_root) else {
            return Err(Error::UnsafePath(entry.path.clone()));
        };
        files.insert(relative.to_owned(), *entry);
        let mut parent = relative.parent();
        while let Some(path) = parent {
            directories.insert(path.to_owned());
            if path.as_os_str().is_empty() {
                break;
            }
            parent = path.parent();
        }
    }
    let mut result = ClonedDirectory {
        files: Vec::with_capacity(files.len()),
        directories: Vec::with_capacity(directories.len()),
    };
    let validation = DirectoryValidation {
        final_root,
        expected_files: &files,
        expected_directories: &directories,
        mask,
        cancelled,
    };
    if !walk_cloned_directory(root, Path::new(""), &validation, &mut result)?
        || result.files.len() != files.len()
        || result.directories.len() != directories.len()
    {
        return Ok(None);
    }
    Ok(Some(result))
}

struct DirectoryValidation<'a> {
    final_root: &'a Path,
    expected_files: &'a HashMap<PathBuf, &'a crate::git::TreeEntry>,
    expected_directories: &'a HashSet<PathBuf>,
    mask: u32,
    cancelled: &'a AtomicUsize,
}

fn walk_cloned_directory(
    root: &Path,
    relative: &Path,
    validation: &DirectoryValidation<'_>,
    result: &mut ClonedDirectory,
) -> Result<bool> {
    if validation.cancelled.load(Ordering::Relaxed) != 0 {
        return Err(Error::Message("worktree creation interrupted".into()));
    }
    if !validation.expected_directories.contains(relative) {
        return Ok(false);
    }
    let path = root.join(relative);
    let directory = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let before = directory.metadata()?;
    if !before.is_dir() || before.st_flags() != 0 || !ordinary_attributes(directory.as_raw_fd()) {
        return Ok(false);
    }
    let wanted_mode = 0o777 & !validation.mask;
    if before.mode() & 0o7777 != wanted_mode
        && unsafe { libc::fchmod(directory.as_raw_fd(), wanted_mode as libc::mode_t) } != 0
    {
        return Err(io::Error::last_os_error().into());
    }

    for item in fs::read_dir(&path)? {
        let item = item?;
        let child = relative.join(item.file_name());
        let kind = item.file_type()?;
        if kind.is_dir() {
            if !walk_cloned_directory(root, &child, validation, result)? {
                return Ok(false);
            }
            continue;
        }
        let Some(expected) = validation.expected_files.get(&child) else {
            return Ok(false);
        };
        if !kind.is_file() {
            return Ok(false);
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(item.path())
        {
            Ok(value) => value,
            Err(_) => return Ok(false),
        };
        let initial = file.metadata()?;
        if !initial.is_file()
            || initial.st_flags() != 0
            || !ordinary_attributes(file.as_raw_fd())
            || !crate::git::blob_matches(&mut file, &expected.oid)?
            || identity(&initial) != identity(&file.metadata()?)
        {
            return Ok(false);
        }
        let wanted_mode = (if expected.executable { 0o777 } else { 0o666 }) & !validation.mask;
        if initial.mode() & 0o7777 != wanted_mode
            && unsafe { libc::fchmod(file.as_raw_fd(), wanted_mode as libc::mode_t) } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if unsafe { libc::futimens(file.as_raw_fd(), std::ptr::null()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let final_path = validation.final_root.join(&child);
        result.files.push((final_path, identity(&file.metadata()?)));
    }
    if unsafe { libc::futimens(directory.as_raw_fd(), std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let meta = directory.metadata()?;
    result.directories.push((
        validation.final_root.join(relative),
        (meta.dev(), meta.ino()),
    ));
    Ok(true)
}

fn start(timings: Option<&CloneTimings>) -> Option<Instant> {
    timings.map(|_| Instant::now())
}

fn finish(timings: Option<&CloneTimings>, phase: ClonePhase, started: Option<Instant>) {
    if let (Some(timings), Some(started)) = (timings, started) {
        timings.record(phase, started.elapsed());
    }
}

pub(super) fn ordinary_attributes(fd: libc::c_int) -> bool {
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
    use sha1::Digest;

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
        let mut directories = CloneDirectoryCache::new();
        let result = clone_new(
            &source,
            &target,
            Path::new("file"),
            0o644,
            &mut directories,
            None,
            |_| {
                fs::write(source.join("file"), "modified during verification")?;
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(result, CloneOutcome::ChangedDuringClone);
        assert!(!target.join("file").exists());
        fs::write(target.join("file"), "external").unwrap();
        assert!(
            clone_new(
                &source,
                &target,
                Path::new("file"),
                0o644,
                &mut directories,
                None,
                |_| Ok(true),
            )
            .is_err()
        );
        assert_eq!(fs::read(target.join("file")).unwrap(), b"external");
    }

    #[test]
    fn directory_clone_never_overwrites_an_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::create_dir_all(target.join("dir")).unwrap();
        fs::write(source.join("dir/file"), "data").unwrap();
        fs::write(target.join("dir/external"), "preserve").unwrap();
        let entry = crate::git::TreeEntry {
            path: PathBuf::from("dir/file"),
            oid: format!("{:x}", sha1::Sha1::digest(b"blob 4\0data")),
            executable: false,
            regular: true,
            gitlink: false,
            tree: false,
        };
        assert!(
            clone_directory_new(
                &source,
                &target,
                Path::new("dir"),
                &[&entry],
                0o022,
                0,
                &AtomicUsize::new(0),
            )
            .is_err()
        );
        assert_eq!(fs::read(target.join("dir/external")).unwrap(), b"preserve");
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    }

    #[test]
    fn cancelled_directory_clone_is_removed_without_publishing() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("dir/file"), "data").unwrap();
        let entry = crate::git::TreeEntry {
            path: PathBuf::from("dir/file"),
            oid: format!("{:x}", sha1::Sha1::digest(b"blob 4\0data")),
            executable: false,
            regular: true,
            gitlink: false,
            tree: false,
        };
        let cancelled = AtomicUsize::new(1);

        let error = match clone_directory_new(
            &source,
            &target,
            Path::new("dir"),
            &[&entry],
            0o022,
            0,
            &cancelled,
        ) {
            Err(error) => error,
            Ok(_) => panic!("cancelled directory clone unexpectedly succeeded"),
        };

        assert_eq!(error.to_string(), "worktree creation interrupted");
        assert!(!target.join("dir").exists());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
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
        let mut directories = CloneDirectoryCache::new();
        assert!(
            clone_new(
                &source,
                &target,
                Path::new("dir/file"),
                0o644,
                &mut directories,
                None,
                |_| Ok(true),
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn removed_destination_parent_is_not_recreated_by_a_worker() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::create_dir_all(target.join("dir")).unwrap();
        fs::write(source.join("dir/file"), "data").unwrap();
        let mut directories = CloneDirectoryCache::new();
        let result = clone_new(
            &source,
            &target,
            Path::new("dir/file"),
            0o644,
            &mut directories,
            None,
            |_| {
                fs::remove_dir(target.join("dir"))?;
                Ok(true)
            },
        );
        assert!(result.is_err());
        assert!(!target.join("dir").exists());
    }

    #[test]
    fn cached_parents_still_detect_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        if SystemPlatform.validate(root.path(), root.path()).is_err() {
            return;
        }
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir_all(source.join("dir")).unwrap();
        fs::create_dir_all(target.join("dir")).unwrap();
        for file in ["first", "second"] {
            fs::write(source.join("dir").join(file), file).unwrap();
        }
        let mut directories = CloneDirectoryCache::new();
        assert_eq!(
            clone_new(
                &source,
                &target,
                Path::new("dir/first"),
                0o644,
                &mut directories,
                None,
                |_| Ok(true),
            )
            .unwrap(),
            CloneOutcome::Cloned
        );

        let displaced = root.path().join("displaced");
        fs::rename(target.join("dir"), &displaced).unwrap();
        fs::create_dir(target.join("dir")).unwrap();
        assert!(
            clone_new(
                &source,
                &target,
                Path::new("dir/second"),
                0o644,
                &mut directories,
                None,
                |_| Ok(true),
            )
            .is_err()
        );
        assert!(!displaced.join("second").exists());
        assert!(!target.join("dir/second").exists());
    }
}
