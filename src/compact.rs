use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Instant,
};

use sha2::{Digest, Sha256};

use crate::{
    eligibility,
    error::{Error, Result},
    git,
    output::{CompactOutcome, CompactResult},
    platform::{self, CloneOutcome, ClonePlatform, SystemPlatform},
    receipt::{self, Receipt, ReceiptState},
    worktree::Worktree,
};

pub fn is_current_receipt(target: &Worktree) -> bool {
    let Ok(located) = receipt::read_for(target) else {
        return false;
    };
    let current = receipt::state_for_target(located.as_ref(), target) == ReceiptState::Compacted;
    if current && let Some(Ok(located)) = located.as_ref() {
        let _ = receipt::migrate_legacy(target, located);
    }
    current
}

pub fn status(target: &Worktree) -> ReceiptState {
    match receipt::read_for(target) {
        Ok(None) => receipt::creation_state(target),
        Ok(located) => receipt::state_for_target(located.as_ref(), target),
        Err(_) => ReceiptState::Unknown,
    }
}

pub fn compact_one(
    donors: &[Worktree],
    target: &Worktree,
    dry_run: bool,
    restricted: bool,
) -> Result<(CompactResult, f64)> {
    let initial_status = status(target);
    let started = Instant::now();
    let platform = SystemPlatform;
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
    let plan = plan(&platform, donors, target, restricted)?;
    let mut cloned = 0u64;
    let mut raced = 0u64;
    if !dry_run {
        let result = clone_paths(&platform, &target.path, &plan.jobs)?;
        cloned = result.0;
        raced = result.1;
        let final_target = git::text(&target.path, ["rev-parse", "--verify", "HEAD"])?;
        if final_target != target.head {
            return Err(Error::Message(
                "target HEAD changed during compaction; no receipt was written".into(),
            ));
        }
        let mut source_commits: Vec<_> = result.2.into_iter().collect();
        source_commits.sort();
        let source_commit = source_commits
            .first()
            .cloned()
            .unwrap_or_else(|| target.head.clone());
        receipt::write(
            target,
            &Receipt {
                source_branch: "multi".into(),
                source_commit,
                source_commits,
                target_only: true,
                target_commit: target.head.clone(),
                excluded_hash: plan.excluded_hash.clone(),
                cloned_files: cloned,
                eligible_allocated_bytes: Some(plan.allocated_bytes),
                completed_at: receipt::now()?,
            },
        )?;
    }
    let outcome = if dry_run {
        CompactOutcome::DryRun
    } else if raced > 0 {
        CompactOutcome::CompactedWithChangedPathsSkipped
    } else {
        CompactOutcome::Compacted
    };
    Ok((
        CompactResult {
            branch: target.branch.clone(),
            path: target.path.to_string_lossy().into_owned(),
            status: if dry_run {
                initial_status
            } else {
                ReceiptState::Compacted
            },
            outcome,
            cloned_files: (!dry_run).then_some(cloned),
            eligible_files: Some(plan.jobs.len() as u64),
            eligible_logical_bytes: Some(plan.logical_bytes),
            eligible_allocated_bytes: Some(plan.allocated_bytes),
            skipped_divergent_paths: Some(plan.excluded_count as u64),
            skipped_changed_paths: (!dry_run).then_some(raced),
            error: None,
        },
        started.elapsed().as_secs_f64(),
    ))
}

struct CloneJob<'a> {
    path: PathBuf,
    oid: String,
    donors: Vec<&'a Worktree>,
}

struct Plan<'a> {
    jobs: Vec<CloneJob<'a>>,
    logical_bytes: u64,
    allocated_bytes: u64,
    excluded_count: usize,
    excluded_hash: String,
}

fn plan<'a>(
    platform: &impl ClonePlatform,
    donors: &'a [Worktree],
    target: &Worktree,
    restricted: bool,
) -> Result<Plan<'a>> {
    let target_entries = git::parse_tree(git::output(
        &target.path,
        ["ls-tree", "-r", "-z", &target.head],
    )?)?;
    let dirty_raw = git::output(
        &target.path,
        [
            "-c",
            "core.trustctime=true",
            "diff",
            "HEAD",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-renames",
            "--",
        ],
    )?;
    let dirty: HashSet<_> = git::nul_paths(&dirty_raw)
        .map(|raw| PathBuf::from(OsString::from_vec(raw.to_vec())))
        .collect();

    let mut usable = Vec::new();
    for donor in donors {
        if donor.path == target.path {
            if restricted {
                return Err(Error::Message(
                    "cannot compact a worktree against itself".into(),
                ));
            }
            continue;
        }
        match platform.validate(&donor.path, &target.path) {
            Ok(()) => usable.push(donor),
            Err(error) if restricted => return Err(error),
            Err(_) => {}
        }
    }
    let mut donors = usable;
    donors.sort_by_key(|donor| (donor.head != target.head, donor.path.clone()));
    let mut inventories = Vec::new();
    for donor in donors {
        let Ok(raw) = git::output(&donor.path, ["ls-tree", "-r", "-z", &donor.head]) else {
            continue;
        };
        let entries = git::parse_tree(raw)?
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<HashMap<_, _>>();
        inventories.push((donor, entries));
    }

    let mut jobs = Vec::new();
    let mut excluded = Vec::new();
    let mut logical_bytes = 0u64;
    let mut allocated_bytes = 0u64;
    let mut safe_target_directories = HashSet::new();
    let mut safe_source_directories = HashMap::<PathBuf, HashSet<PathBuf>>::new();
    for entry in target_entries {
        if !entry.regular || dirty.contains(&entry.path) {
            excluded.push(entry.path);
            continue;
        }
        eligibility::validate_ancestors(&target.path, &entry.path, &mut safe_target_directories)?;
        let Ok(metadata) = fs::symlink_metadata(target.path.join(&entry.path)) else {
            excluded.push(entry.path);
            continue;
        };
        if !metadata.file_type().is_file() {
            excluded.push(entry.path);
            continue;
        }
        let mut matching = Vec::new();
        for (donor, inventory) in &inventories {
            let Some(candidate) = inventory
                .get(&entry.path)
                .filter(|candidate| candidate.regular && candidate.oid == entry.oid)
            else {
                continue;
            };
            let safe = safe_source_directories
                .entry(donor.path.clone())
                .or_default();
            if eligibility::validate_ancestors(&donor.path, &candidate.path, safe).is_ok() {
                matching.push(*donor);
            }
        }
        if matching.is_empty() {
            excluded.push(entry.path);
            continue;
        }
        logical_bytes = logical_bytes.saturating_add(metadata.len());
        allocated_bytes = allocated_bytes.saturating_add(metadata.blocks().saturating_mul(512));
        jobs.push(CloneJob {
            path: entry.path,
            oid: entry.oid,
            donors: matching,
        });
    }
    excluded.sort();
    let mut hasher = Sha256::new();
    for path in &excluded {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update([0]);
    }
    Ok(Plan {
        jobs,
        logical_bytes,
        allocated_bytes,
        excluded_count: excluded.len(),
        excluded_hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn clone_paths(
    platform: &(impl ClonePlatform + Sync),
    target: &Path,
    jobs: &[CloneJob<'_>],
) -> Result<(u64, u64, HashSet<String>)> {
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    clone_paths_with_workers(platform, target, jobs, workers)
}

fn clone_paths_with_workers(
    platform: &(impl ClonePlatform + Sync),
    target: &Path,
    jobs: &[CloneJob<'_>],
    workers: usize,
) -> Result<(u64, u64, HashSet<String>)> {
    // Small worktrees don't amortize thread startup. Bound filesystem concurrency
    // rather than creating one worker per file or per worktree.
    let workers = workers.min((jobs.len() / 256).max(1)).max(1);
    let chunk_size = jobs.len().div_ceil(workers).max(1);
    let cancelled = AtomicBool::new(false);
    let clone_chunk = |chunk: &[CloneJob<'_>], offset: usize| {
        let mut cloned = 0;
        let mut raced = 0;
        let mut sources = HashSet::new();
        for (index, job) in chunk.iter().enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            let mut completed = false;
            for donor in &job.donors {
                match platform.clone_replacing(
                    &donor.path,
                    target,
                    &job.path,
                    &job.oid,
                    (offset + index) as u64,
                ) {
                    Ok(CloneOutcome::Cloned) => {
                        cloned += 1;
                        sources.insert(donor.head.clone());
                        completed = true;
                        break;
                    }
                    Ok(CloneOutcome::ChangedDuringClone) => {
                        raced += 1;
                        completed = true;
                        break;
                    }
                    Ok(CloneOutcome::NotRegular) => {}
                    Err(error) => {
                        cancelled.store(true, Ordering::Relaxed);
                        return Err(error);
                    }
                }
            }
            if !completed {
                raced += 1;
            }
        }
        Ok((cloned, raced, sources))
    };
    if workers == 1 {
        return clone_chunk(jobs, 0);
    }
    thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .chunks(chunk_size)
            .enumerate()
            .map(|(index, chunk)| {
                let clone_chunk = &clone_chunk;
                scope.spawn(move || clone_chunk(chunk, index * chunk_size))
            })
            .collect();
        // Join every worker before returning, including on error: no background
        // replacements may outlive final validation or receipt creation.
        let mut total = (0, 0, HashSet::new());
        let mut error = None;
        for handle in handles {
            match handle
                .join()
                .unwrap_or_else(|_| Err(Error::Message("compaction worker panicked".into())))
            {
                Ok((cloned, raced, sources)) => {
                    total.0 += cloned;
                    total.1 += raced;
                    total.2.extend(sources);
                }
                Err(value) => {
                    error.get_or_insert(value);
                }
            }
        }
        error.map_or(Ok(total), Err)
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
            _: &str,
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

    fn jobs<'a>(paths: &[PathBuf], donor: &'a Worktree) -> Vec<CloneJob<'a>> {
        paths
            .iter()
            .map(|path| CloneJob {
                path: path.clone(),
                oid: String::new(),
                donors: vec![donor],
            })
            .collect()
    }

    #[test]
    fn parallel_clones_visit_each_path_once_and_count_outcomes() {
        let paths: Vec<_> = (0..1025).map(|i| PathBuf::from(i.to_string())).collect();
        let platform = RecordingPlatform {
            calls: (0..paths.len()).map(|_| AtomicUsize::new(0)).collect(),
            fail: false,
        };
        let donor = Worktree {
            path: PathBuf::from("s"),
            head: "source".into(),
            branch: None,
            locked: false,
        };
        let jobs = jobs(&paths, &donor);
        let result = clone_paths_with_workers(&platform, Path::new("t"), &jobs, 4).unwrap();
        assert_eq!((result.0, result.1), (342, 683));
        assert_eq!(result.2, HashSet::from(["source".into()]));
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
        let donor = Worktree {
            path: PathBuf::from("s"),
            head: "source".into(),
            branch: None,
            locked: false,
        };
        let jobs = jobs(&paths, &donor);
        let error = clone_paths_with_workers(&platform, Path::new("t"), &jobs, 4).unwrap_err();
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
