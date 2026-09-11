use std::{io, path::PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Git command failed: {0}")]
    Git(String),
    #[error("Git command failed: {message}")]
    GitProcess { code: i32, message: String },
    #[error("not inside a Git worktree")]
    NotWorktree,
    #[error("worktree not found: {0}")]
    WorktreeNotFound(String),
    #[error("ambiguous worktree: {0}")]
    AmbiguousWorktree(String),
    #[error("source and target are not on the same filesystem volume")]
    CrossVolume,
    #[error("filesystem is not supported: {0}")]
    UnsupportedFilesystem(String),
    #[error("unsafe Git path: {0:?}")]
    UnsafePath(PathBuf),
    #[error("clone failed for {path:?}: {source}")]
    Clone { path: PathBuf, source: io::Error },
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
