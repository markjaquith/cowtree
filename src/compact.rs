use std::time::Instant;

use crate::{
    eligibility,
    error::{Error, Result},
    git,
    output::{CompactResult, EstimateResult},
    platform::{CloneOutcome, ClonePlatform, SystemPlatform},
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
    let eligible = eligibility::calculate(source, target, &source_commit)?;
    let mut cloned = 0u64;
    let mut raced = 0u64;
    if !dry_run {
        for (sequence, path) in eligible.paths.iter().enumerate() {
            match platform.clone_replacing(&source.path, &target.path, path, sequence as u64)? {
                CloneOutcome::Cloned => cloned += 1,
                CloneOutcome::ChangedDuringClone => raced += 1,
                CloneOutcome::NotRegular => {}
            }
        }
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
