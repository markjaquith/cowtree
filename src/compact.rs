use std::{
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Instant,
};

use crate::{
    eligibility,
    error::{Error, Result},
    git,
    output::{CompactResult, EstimateResult},
    platform::{self, CloneOutcome, ClonePlatform, SystemPlatform},
    receipt::{self, Receipt, ReceiptState},
    worktree::Worktree,
};

pub fn validate_source(source: &Worktree) -> Result<String> {
    let branch = source.branch.as_ref().ok_or(Error::DetachedSource)?;
    let reference = format!("refs/heads/{branch}^{{commit}}");
    let commit = git::text(&source.path, ["rev-parse", "--verify", reference.as_str()])?;
    if source.head != commit {
        return Err(Error::SourceHeadMismatch(branch.clone()));
    }
    let dirty = git::output(
        &source.path,
        [
            "-c",
            "core.trustctime=true",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=no",
        ],
    )?;
    if !dirty.is_empty() {
        return Err(Error::DirtySource(source.path.clone()));
    }
    Ok(commit)
}

pub fn is_current_receipt(source: &Worktree, target: &Worktree) -> bool {
    let Ok(located) = receipt::read_for(target) else {
        return false;
    };
    let current =
        receipt::state(located.as_ref(), &source.head, &target.head) == ReceiptState::Compacted;
    if current && let Some(Ok(located)) = located.as_ref() {
        let _ = receipt::migrate_legacy(target, located);
    }
    current
}

pub fn compact_one(
    source: &Worktree,
    target: &Worktree,
    dry_run: bool,
) -> Result<(CompactResult, f64)> {
    if source.path == target.path {
        return Err(Error::Message(
            "cannot compact the source worktree against itself".into(),
        ));
    }
    let started = Instant::now();
    let source_commit = validate_source(source)?;
    let platform = SystemPlatform;
    platform.validate(&source.path, &target.path)?;
    if !dry_run {
        let tracked = git::output(&target.path, ["ls-files", "--cached", "-z"])?;
        let tracked: Vec<_> = git::nul_paths(&tracked)
            .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())))
            .collect();
        let untracked = git::output(
            &target.path,
            ["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let untracked: Vec<_> = git::nul_paths(&untracked)
            .map(|path| PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())))
            .collect();
        platform::cleanup_stale_clones(&target.path, &tracked, &untracked)?;
    }
    let eligible = eligibility::calculate(source, target, &source_commit)?;
    let mut cloned = 0u64;
    let mut raced = 0u64;
    if !dry_run {
        (cloned, raced) = clone_paths(&platform, &source.path, &target.path, &eligible.paths)?;
        let final_source = validate_source(source)?;
        let final_target = git::text(&target.path, ["rev-parse", "--verify", "HEAD"])?;
        if final_source != source_commit || final_target != target.head {
            return Err(Error::Message(
                "source or target HEAD changed during compaction; no receipt was written".into(),
            ));
        }
        receipt::write(
            target,
            &Receipt {
                source_branch: source.branch.clone().ok_or(Error::DetachedSource)?,
                source_commit,
                target_commit: target.head.clone(),
                excluded_hash: eligible.excluded_hash.clone(),
                cloned_files: cloned,
                eligible_allocated_bytes: Some(eligible.allocated_bytes),
                completed_at: receipt::now()?,
            },
        )?;
    }
    let outcome = if dry_run {
        "dry_run"
    } else if raced > 0 {
        "compacted_with_race_skips"
    } else {
        "compacted"
    };
    Ok((
        CompactResult {
            worktree: target.path.to_string_lossy().into_owned(),
            label: target.label(),
            outcome: outcome.into(),
            cloned_files: cloned,
            eligible_files: eligible.paths.len() as u64,
            eligible_logical_bytes: eligible.logical_bytes,
            eligible_allocated_bytes: eligible.allocated_bytes,
            skipped_divergent_paths: eligible.excluded_count as u64,
            error: None,
        },
        started.elapsed().as_secs_f64(),
    ))
}

fn clone_paths(
    platform: &(impl ClonePlatform + Sync),
    source: &Path,
    target: &Path,
    paths: &[PathBuf],
) -> Result<(u64, u64)> {
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    clone_paths_with_workers(platform, source, target, paths, workers)
}

fn clone_paths_with_workers(
    platform: &(impl ClonePlatform + Sync),
    source: &Path,
    target: &Path,
    paths: &[PathBuf],
    workers: usize,
) -> Result<(u64, u64)> {
    // Small worktrees don't amortize thread startup. Bound filesystem concurrency
    // rather than creating one worker per file or per worktree.
    let workers = workers.min((paths.len() / 256).max(1)).max(1);
    let chunk_size = paths.len().div_ceil(workers).max(1);
    let cancelled = AtomicBool::new(false);
    let clone_chunk = |chunk: &[PathBuf], offset: usize| -> Result<(u64, u64)> {
        let mut cloned = 0;
        let mut raced = 0;
        for (index, path) in chunk.iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            match platform.clone_replacing(source, target, path, (offset + index) as u64) {
                Ok(CloneOutcome::Cloned) => cloned += 1,
                Ok(CloneOutcome::ChangedDuringClone) => raced += 1,
                Ok(CloneOutcome::NotRegular) => {}
                Err(error) => {
                    cancelled.store(true, Ordering::Relaxed);
                    return Err(error);
                }
            }
        }
        Ok((cloned, raced))
    };
    if workers == 1 {
        return clone_chunk(paths, 0);
    }
    thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, chunk)| {
                let clone_chunk = &clone_chunk;
                scope.spawn(move || clone_chunk(chunk, index * chunk_size))
            })
            .collect();
        // Join every worker before returning, including on error: no background
        // replacements may outlive final validation or receipt creation.
        let mut total = (0, 0);
        let mut error = None;
        for handle in handles {
            match handle
                .join()
                .unwrap_or_else(|_| Err(Error::Message("compaction worker panicked".into())))
            {
                Ok((cloned, raced)) => {
                    total.0 += cloned;
                    total.1 += raced;
                }
                Err(value) => {
                    error.get_or_insert(value);
                }
            }
        }
        error.map_or(Ok(total), Err)
    })
}

pub fn estimate_one(source: &Worktree, target: &Worktree) -> Result<EstimateResult> {
    if source.path == target.path {
        return Err(Error::Message(
            "cannot estimate the source worktree against itself".into(),
        ));
    }
    let source_commit = validate_source(source)?;
    SystemPlatform.validate(&source.path, &target.path)?;
    let eligible = eligibility::calculate(source, target, &source_commit)?;
    Ok(EstimateResult {
        worktree: target.path.to_string_lossy().into_owned(),
        label: target.label(),
        eligible_files: eligible.paths.len() as u64,
        eligible_logical_bytes: eligible.logical_bytes,
        eligible_allocated_bytes: eligible.allocated_bytes,
        skipped_divergent_paths: eligible.excluded_count as u64,
        current_receipt: is_current_receipt(source, target),
        outcome: "estimated".into(),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct RecordingPlatform {
        calls: Vec<AtomicUsize>,
        fail: bool,
    }

    impl ClonePlatform for RecordingPlatform {
        fn validate(&self, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }

        fn clone_replacing(
            &self,
            _: &Path,
            _: &Path,
            relative: &Path,
            sequence: u64,
        ) -> Result<CloneOutcome> {
            let index: usize = relative.to_str().unwrap().parse().unwrap();
            assert_eq!(sequence as usize, index);
            self.calls[index].fetch_add(1, Ordering::Relaxed);
            if self.fail && index == 0 {
                return Err(Error::Message("injected clone failure".into()));
            }
            Ok(match index % 3 {
                0 => CloneOutcome::Cloned,
                1 => CloneOutcome::ChangedDuringClone,
                _ => CloneOutcome::NotRegular,
            })
        }
    }

    #[test]
    fn parallel_clones_visit_each_path_once_and_count_outcomes() {
        let paths: Vec<_> = (0..1025).map(|i| PathBuf::from(i.to_string())).collect();
        let platform = RecordingPlatform {
            calls: (0..paths.len()).map(|_| AtomicUsize::new(0)).collect(),
            fail: false,
        };
        let result =
            clone_paths_with_workers(&platform, Path::new("s"), Path::new("t"), &paths, 4).unwrap();
        assert_eq!(result, (342, 342));
        assert!(
            platform
                .calls
                .iter()
                .all(|count| count.load(Ordering::Relaxed) == 1)
        );
    }

    #[test]
    fn parallel_clone_failure_is_propagated() {
        let paths: Vec<_> = (0..1024).map(|i| PathBuf::from(i.to_string())).collect();
        let platform = RecordingPlatform {
            calls: (0..paths.len()).map(|_| AtomicUsize::new(0)).collect(),
            fail: true,
        };
        let error = clone_paths_with_workers(&platform, Path::new("s"), Path::new("t"), &paths, 4)
            .unwrap_err();
        assert_eq!(error.to_string(), "injected clone failure");
        assert!(
            platform
                .calls
                .iter()
                .all(|count| count.load(Ordering::Relaxed) <= 1)
        );
        assert_eq!(platform.calls[0].load(Ordering::Relaxed), 1);
    }
}
