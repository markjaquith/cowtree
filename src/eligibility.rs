use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Component, Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    error::{Error, Result},
    git,
    worktree::Worktree,
};

#[derive(Debug)]
pub struct Eligibility {
    pub paths: Vec<PathBuf>,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub excluded_count: usize,
    pub excluded_hash: String,
}

pub fn calculate(source: &Worktree, target: &Worktree, source_commit: &str) -> Result<Eligibility> {
    let candidates = git::output(&source.path, ["ls-files", "--cached", "-z"])?;
    let changed = git::output(
        &target.path,
        [
            "-c",
            "core.trustctime=true",
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-renames",
            source_commit,
            "--",
        ],
    )?;
    let excluded: HashSet<Vec<u8>> = git::nul_paths(&changed)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    let mut hasher = Sha256::new();
    let mut sorted_excluded: Vec<_> = excluded.iter().collect();
    sorted_excluded.sort_unstable();
    for path in sorted_excluded {
        hasher.update(path);
        hasher.update([0]);
    }

    let mut paths = Vec::new();
    let mut logical_bytes = 0u64;
    let mut allocated_bytes = 0u64;
    let mut safe_source_directories = HashSet::new();
    let mut safe_target_directories = HashSet::new();
    for raw in git::nul_paths(&candidates) {
        if excluded.contains(raw) {
            continue;
        }
        let relative = PathBuf::from(OsString::from_vec(raw.to_vec()));
        validate_relative(&relative)?;
        validate_ancestors(&source.path, &relative, &mut safe_source_directories)?;
        validate_ancestors(&target.path, &relative, &mut safe_target_directories)?;
        let source_path = source.path.join(&relative);
        let target_path = target.path.join(&relative);
        let (Ok(source_meta), Ok(target_meta)) = (
            fs::symlink_metadata(source_path),
            fs::symlink_metadata(target_path),
        ) else {
            continue;
        };
        if !source_meta.file_type().is_file() || !target_meta.file_type().is_file() {
            continue;
        }
        logical_bytes = logical_bytes.saturating_add(target_meta.len());
        allocated_bytes = allocated_bytes.saturating_add(target_meta.blocks().saturating_mul(512));
        paths.push(relative);
    }
    Ok(Eligibility {
        paths,
        logical_bytes,
        allocated_bytes,
        excluded_count: excluded.len(),
        excluded_hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn validate_ancestors(
    root: &Path,
    relative: &Path,
    known_safe: &mut HashSet<PathBuf>,
) -> Result<()> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = root.to_owned();
    for component in parent.components() {
        current.push(component);
        if known_safe.contains(&current) {
            continue;
        }
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| Error::UnsafePath(relative.to_owned()))?;
        if !metadata.file_type().is_dir() {
            return Err(Error::UnsafePath(relative.to_owned()));
        }
        known_safe.insert(current.clone());
    }
    Ok(())
}

pub fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::UnsafePath(path.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        assert!(validate_relative(Path::new("ok/file")).is_ok());
        assert!(validate_relative(Path::new("../outside")).is_err());
        assert!(validate_relative(Path::new("/outside")).is_err());
    }

    #[test]
    fn rejects_symlinked_ancestor_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(
            validate_ancestors(root.path(), Path::new("escape/file"), &mut HashSet::new()).is_err()
        );
    }
}
