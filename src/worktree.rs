use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

use crate::{
    error::{Error, Result},
    git,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub locked: bool,
}

impl Worktree {
    pub fn label(&self) -> String {
        self.branch
            .clone()
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

pub fn discover(cwd: &Path) -> Result<Vec<Worktree>> {
    let raw = git::output(cwd, ["worktree", "list", "--porcelain", "-z"]).map_err(|error| {
        if matches!(error, Error::Git(_)) {
            Error::NotWorktree
        } else {
            error
        }
    })?;
    parse_porcelain(&raw)
}

pub fn parse_porcelain(raw: &[u8]) -> Result<Vec<Worktree>> {
    let mut result = Vec::new();
    let mut path = None;
    let mut head = None;
    let mut branch = None;
    let mut locked = false;

    for field in raw.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let (Some(path), Some(head)) = (path.take(), head.take()) {
                result.push(Worktree {
                    path,
                    head,
                    branch: branch.take(),
                    locked,
                });
                locked = false;
            }
            continue;
        }
        if let Some(value) = field.strip_prefix(b"worktree ") {
            #[cfg(unix)]
            {
                path = Some(PathBuf::from(OsString::from_vec(value.to_vec())));
            }
        } else if let Some(value) = field.strip_prefix(b"HEAD ") {
            head = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(value) = field.strip_prefix(b"branch ") {
            branch = Some(
                String::from_utf8_lossy(value.strip_prefix(b"refs/heads/").unwrap_or(value))
                    .into_owned(),
            );
        } else if field == b"locked" || field.starts_with(b"locked ") {
            locked = true;
        }
    }
    if let (Some(path), Some(head)) = (path, head) {
        result.push(Worktree {
            path,
            head,
            branch,
            locked,
        });
    }
    Ok(result)
}

pub fn resolve<'a>(worktrees: &'a [Worktree], value: &Path) -> Result<&'a Worktree> {
    let path_matches: Vec<_> = worktrees
        .iter()
        .filter(|wt| same_path(&wt.path, value))
        .collect();
    if path_matches.len() == 1 {
        return Ok(path_matches[0]);
    }
    let text = value.to_string_lossy();
    let branch_matches: Vec<_> = worktrees
        .iter()
        .filter(|wt| wt.branch.as_deref() == Some(text.as_ref()))
        .collect();
    match branch_matches.as_slice() {
        [worktree] => Ok(worktree),
        [] => Err(Error::WorktreeNotFound(text.into_owned())),
        _ => Err(Error::AmbiguousWorktree(text.into_owned())),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

pub fn default_source(worktrees: &[Worktree], cwd: &Path) -> Result<Worktree> {
    let inferred = git::text(
        cwd,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()
    .and_then(|name| name.strip_prefix("origin/").map(str::to_owned))
    .or_else(|| {
        ["main", "master"]
            .into_iter()
            .find(|name| {
                let reference = format!("refs/heads/{name}^{{commit}}");
                git::text(cwd, ["rev-parse", "--verify", &reference]).is_ok()
            })
            .map(str::to_owned)
    })
    .ok_or_else(|| {
        Error::Message("could not infer source branch from origin/HEAD, main, or master".into())
    })?;
    if !worktrees
        .iter()
        .any(|worktree| worktree.branch.as_deref() == Some(&inferred))
    {
        return Err(Error::Message(format!(
            "default source branch '{inferred}' is not checked out; create a worktree for it or pass --source <checked-out-branch-or-path>"
        )));
    }
    resolve(worktrees, Path::new(&inferred)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_detached_and_locked_worktrees() {
        let raw = b"worktree /repo\0HEAD abc\0branch refs/heads/main\0\0worktree /tmp/wt\0HEAD def\0detached\0locked reason\0\0";
        let parsed = parse_porcelain(raw).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].branch, None);
        assert!(parsed[1].locked);
    }
}
