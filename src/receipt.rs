use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    error::{Error, Result},
    git,
    worktree::Worktree,
};

pub const CANONICAL_NAME: &str = "cowtree-compaction";
pub const LEGACY_NAME: &str = "wt-prewarm-compaction";
pub const CREATION_NAME: &str = "cowtree-creation";

#[derive(serde::Serialize, serde::Deserialize)]
struct CreationReceipt {
    version: u32,
    operation: String,
    target_commit: String,
    source_commits: Vec<String>,
    cloned_files: u64,
    completed_at: String,
}

pub fn write_creation(
    admin: &std::path::Path,
    target: &str,
    sources: &[String],
    cloned_files: u64,
) -> Result<()> {
    let receipt = CreationReceipt {
        version: 1,
        operation: "create".into(),
        target_commit: target.into(),
        source_commits: sources.to_vec(),
        cloned_files,
        completed_at: now()?,
    };
    let temporary = admin.join(format!(".{CREATION_NAME}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, &receipt)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, admin.join(CREATION_NAME))?;
        File::open(admin)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn creation_state(worktree: &Worktree) -> ReceiptState {
    let Ok(admin) = admin_dir(worktree) else {
        return ReceiptState::Unknown;
    };
    let body = match fs::read(admin.join(CREATION_NAME)) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ReceiptState::NotCompacted;
        }
        Err(_) => return ReceiptState::Unknown,
    };
    let Ok(receipt) = serde_json::from_slice::<CreationReceipt>(&body) else {
        return ReceiptState::Invalid;
    };
    if receipt.version != 1
        || receipt.operation != "create"
        || receipt.source_commits.is_empty()
        || receipt.cloned_files == 0
        || !valid_oid(&receipt.target_commit)
        || receipt
            .source_commits
            .iter()
            .any(|oid| !valid_oid(oid) || oid.len() != receipt.target_commit.len())
        || receipt.completed_at.is_empty()
    {
        return ReceiptState::Invalid;
    }
    if receipt.target_commit == worktree.head {
        ReceiptState::Created
    } else {
        ReceiptState::Stale
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub source_branch: String,
    pub source_commit: String,
    pub target_commit: String,
    pub excluded_hash: String,
    pub cloned_files: u64,
    pub eligible_allocated_bytes: Option<u64>,
    pub completed_at: String,
}

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Compacted,
    Created,
    Stale,
    NotCompacted,
    Invalid,
    Unknown,
}

#[derive(Debug)]
pub struct LocatedReceipt {
    pub receipt: Receipt,
    pub legacy: bool,
}

pub fn admin_dir(worktree: &Worktree) -> Result<PathBuf> {
    Ok(PathBuf::from(git::text(
        &worktree.path,
        ["rev-parse", "--absolute-git-dir"],
    )?))
}

pub fn read_for(worktree: &Worktree) -> Result<Option<std::result::Result<LocatedReceipt, Error>>> {
    let admin = match admin_dir(worktree) {
        Ok(path) => path,
        Err(error) => return Ok(Some(Err(error))),
    };
    for (name, legacy) in [(CANONICAL_NAME, false), (LEGACY_NAME, true)] {
        let path = admin.join(name);
        if path.exists() {
            let content = match fs::read(&path) {
                Ok(value) => value,
                Err(error) => return Ok(Some(Err(error.into()))),
            };
            return Ok(Some(
                parse(&content).map(|receipt| LocatedReceipt { receipt, legacy }),
            ));
        }
    }
    Ok(None)
}

pub fn parse(content: &[u8]) -> Result<Receipt> {
    let text =
        std::str::from_utf8(content).map_err(|_| Error::Message("receipt is not UTF-8".into()))?;
    let mut fields = HashMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| Error::Message("malformed receipt line".into()))?;
        if fields.insert(key, value).is_some() {
            return Err(Error::Message(format!("duplicate receipt field: {key}")));
        }
    }
    if fields.get("version") != Some(&"1") {
        return Err(Error::Message("unsupported receipt version".into()));
    }
    let required = |key: &str| {
        fields
            .get(key)
            .copied()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Message(format!("receipt is missing {key}")))
    };
    Ok(Receipt {
        source_branch: required("source_branch")?.to_owned(),
        source_commit: required("source_commit")?.to_owned(),
        target_commit: required("target_commit")?.to_owned(),
        excluded_hash: required("excluded_hash")?.to_owned(),
        cloned_files: required("cloned_files")?
            .parse()
            .map_err(|_| Error::Message("invalid cloned_files".into()))?,
        eligible_allocated_bytes: fields
            .get("eligible_allocated_bytes")
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| Error::Message("invalid eligible_allocated_bytes".into()))
            })
            .transpose()?,
        completed_at: required("completed_at")?.to_owned(),
    })
}

pub fn write(worktree: &Worktree, receipt: &Receipt) -> Result<()> {
    let admin = admin_dir(worktree)?;
    let destination = admin.join(CANONICAL_NAME);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = admin.join(format!(
        ".{CANONICAL_NAME}.tmp.{}.{}",
        std::process::id(),
        nonce
    ));
    let mut body = format!(
        "version=1\nsource_branch={}\nsource_commit={}\ntarget_commit={}\nexcluded_hash={}\ncloned_files={}\n",
        receipt.source_branch,
        receipt.source_commit,
        receipt.target_commit,
        receipt.excluded_hash,
        receipt.cloned_files,
    );
    if let Some(bytes) = receipt.eligible_allocated_bytes {
        body.push_str(&format!("eligible_allocated_bytes={bytes}\n"));
    }
    body.push_str(&format!("completed_at={}\n", receipt.completed_at));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        File::open(&admin)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn migrate_legacy(worktree: &Worktree, located: &LocatedReceipt) -> Result<()> {
    if !located.legacy {
        return Ok(());
    }
    write(worktree, &located.receipt)?;
    let admin = admin_dir(worktree)?;
    match fs::remove_file(admin.join(LEGACY_NAME)) {
        Ok(()) => File::open(admin)?.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn now() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| Error::Message(error.to_string()))
}

pub fn state(
    located: Option<&std::result::Result<LocatedReceipt, Error>>,
    source_commit: &str,
    target_commit: &str,
) -> ReceiptState {
    match located {
        None => ReceiptState::NotCompacted,
        Some(Err(_)) => ReceiptState::Invalid,
        Some(Ok(value))
            if value.receipt.source_commit == source_commit
                && value.receipt.target_commit == target_commit =>
        {
            ReceiptState::Compacted
        }
        Some(Ok(_)) => ReceiptState::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &[u8] = b"version=1\nsource_branch=main\nsource_commit=abc\ntarget_commit=def\nexcluded_hash=123\ncloned_files=4\ncompleted_at=2026-01-01T00:00:00Z\n";

    #[test]
    fn parses_legacy_core_fields() {
        let value = parse(LEGACY).unwrap();
        assert_eq!(value.cloned_files, 4);
        assert_eq!(value.eligible_allocated_bytes, None);
    }

    #[test]
    fn rejects_unsupported_receipts() {
        assert!(parse(b"version=2\n").is_err());
    }

    #[test]
    fn computes_state_transitions() {
        let located = Ok(LocatedReceipt {
            receipt: parse(LEGACY).unwrap(),
            legacy: true,
        });
        assert_eq!(state(Some(&located), "abc", "def"), ReceiptState::Compacted);
        assert_eq!(state(Some(&located), "new", "def"), ReceiptState::Stale);
        assert_eq!(state(None, "abc", "def"), ReceiptState::NotCompacted);
    }
}
