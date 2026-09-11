use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

use crate::error::{Error, Result};

pub(crate) fn validate_ancestors(
    root: &Path,
    relative: &Path,
    known_safe: &mut HashSet<PathBuf>,
) -> Result<()> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    if known_safe.contains(&root.join(parent)) {
        return Ok(());
    }
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
