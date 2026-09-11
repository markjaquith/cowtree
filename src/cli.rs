use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use usage::{Args, Cli as UsageCli, Subcommands};

use crate::{
    add, compact,
    error::{Error, Result},
    output::{
        self, CompactOutcome, CompactResult, CompactSummary, CompactUi, Envelope, StatusResult,
    },
    receipt::{self, ReceiptState},
    worktree::{self, Worktree},
};

#[derive(Debug, UsageCli)]
#[usage(
    bin = "cowtree",
    version,
    about = "Create and compact Git worktrees with copy-on-write clones",
    unknown_flags = "error",
    args_override_self = false
)]
pub struct Cli {
    #[usage(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommands)]
enum Command {
    /// Create a copy-on-write worktree
    Add(AddArgs),
    Compact(OperationArgs),
    Status(StatusArgs),
}

#[derive(Debug, Args)]
#[usage(disable_help_flag = true, unknown_flags = "value")]
struct AddArgs {
    /// Arguments accepted by `git worktree add`
    #[usage(name = "ARG", double_dash = "preserve")]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
#[usage(args_override_self = false)]
struct OperationArgs {
    /// Branch or registered worktree path
    target: Option<PathBuf>,
    /// Process every linked worktree except the current or restricted source
    #[usage(long, conflicts("target"))]
    all: bool,
    /// Restrict donors to one checked-out branch or worktree path
    #[usage(long)]
    source: Option<PathBuf>,
    /// Emit stable, versioned JSON
    #[usage(long)]
    json: bool,
    /// Calculate candidates without cloning (compact only)
    #[usage(long)]
    dry_run: bool,
    /// Reprocess targets with current compaction receipts
    #[usage(long)]
    recompact: bool,
}

#[derive(Debug, Args)]
#[usage(args_override_self = false)]
struct StatusArgs {
    /// Branch or registered worktree path (defaults to the current worktree)
    target: Option<PathBuf>,
    #[usage(long, conflicts("target"))]
    all: bool,
    #[usage(long)]
    json: bool,
}

pub fn run(cli: Cli) -> Result<i32> {
    let command = match cli.command {
        Command::Add(args) => return add::run(args.args),
        command => command,
    };
    let cwd = env::current_dir()?;
    let worktrees = worktree::discover(&cwd)?;
    match command {
        Command::Compact(args) => run_compact(&cwd, &worktrees, args)?,
        Command::Status(args) => run_status(&cwd, &worktrees, args)?,
        Command::Add(_) => unreachable!(),
    };
    Ok(0)
}

fn targets_for<'a>(
    cwd: &Path,
    worktrees: &'a [Worktree],
    target: Option<&PathBuf>,
    all: bool,
    source: Option<&Worktree>,
) -> Result<Vec<&'a Worktree>> {
    if all {
        return Ok(worktrees
            .iter()
            .filter(|target| source.is_none_or(|source| target.path != source.path))
            .collect());
    }
    let requested = target.map(PathBuf::as_path).unwrap_or(cwd);
    Ok(vec![worktree::resolve(worktrees, requested)?])
}

fn run_compact(cwd: &Path, worktrees: &[Worktree], args: OperationArgs) -> Result<()> {
    if !args.all && args.target.is_none() {
        return Err(Error::Message("compact requires a target or --all".into()));
    }
    let restricted = args
        .source
        .as_ref()
        .map(|source| worktree::resolve(worktrees, source).cloned())
        .transpose()?;
    let current = worktree::resolve(worktrees, cwd).ok();
    let targets = targets_for(
        cwd,
        worktrees,
        args.target.as_ref(),
        args.all,
        restricted.as_ref().or(current),
    )?;
    let donors: Vec<_> = restricted.map_or_else(|| worktrees.to_vec(), |source| vec![source]);
    let total = targets.len();
    let mut results = Vec::new();
    let mut summary = CompactSummary::default();
    let mut ui = CompactUi::new();
    for (index, target) in targets.into_iter().enumerate() {
        if args.all && !args.recompact && compact::is_current_receipt(target) {
            summary.already_compacted += 1;
            if !args.json {
                ui.already_compacted(index + 1, total, &target.label());
            }
            results.push(CompactResult {
                branch: target.branch.clone(),
                path: target.path.to_string_lossy().into_owned(),
                status: ReceiptState::Compacted,
                outcome: CompactOutcome::AlreadyCompacted,
                cloned_files: None,
                eligible_files: None,
                eligible_logical_bytes: None,
                eligible_allocated_bytes: None,
                skipped_divergent_paths: None,
                skipped_changed_paths: None,
                error: None,
            });
            continue;
        }
        if !args.json {
            ui.start(index + 1, total, &target.label(), args.dry_run);
        }
        match compact::compact_one(&donors, target, args.dry_run, args.source.is_some()) {
            Ok((result, seconds)) => {
                summary.compacted += usize::from(!args.dry_run);
                summary.dry_run += usize::from(args.dry_run);
                if !args.json {
                    ui.success(index + 1, total, &result, seconds, args.dry_run);
                }
                results.push(result);
            }
            Err(error) => {
                summary.failed += 1;
                if !args.json {
                    ui.failed(index + 1, total, &target.label(), &error);
                }
                results.push(CompactResult {
                    branch: target.branch.clone(),
                    path: target.path.to_string_lossy().into_owned(),
                    status: compact::status(target),
                    outcome: CompactOutcome::Failed,
                    cloned_files: None,
                    eligible_files: None,
                    eligible_logical_bytes: None,
                    eligible_allocated_bytes: None,
                    skipped_divergent_paths: None,
                    skipped_changed_paths: None,
                    error: Some(error.to_string()),
                });
                if !args.all {
                    break;
                }
            }
        }
    }
    if args.json {
        print_json("compact", results, Some(summary))?;
    } else {
        ui.summary(summary);
    }
    if summary.failed > 0 {
        Err(Error::Message(format!(
            "{} target(s) failed",
            summary.failed
        )))
    } else {
        Ok(())
    }
}

fn run_status(cwd: &Path, worktrees: &[Worktree], args: StatusArgs) -> Result<()> {
    let current = worktree::resolve(worktrees, cwd).ok();
    let targets = targets_for(cwd, worktrees, args.target.as_ref(), args.all, current)?;
    let mut results = Vec::new();
    for target in targets {
        let located = receipt::read_for(target)?;
        let state = match located.as_ref() {
            None => receipt::creation_state(target),
            Some(Err(_)) => ReceiptState::Invalid,
            Some(Ok(value)) => {
                let state = receipt::state_for_target(located.as_ref(), target);
                if state == ReceiptState::Compacted {
                    let _ = receipt::migrate_legacy(target, value);
                }
                state
            }
        };
        let result = StatusResult {
            branch: target.branch.clone(),
            path: target.path.to_string_lossy().into_owned(),
            status: state,
        };
        results.push(result);
    }
    if args.json {
        print_json("status", results, None)?;
    } else {
        output::print_status(&results);
    }
    Ok(())
}

fn print_json<T: serde::Serialize>(
    command: &'static str,
    results: Vec<T>,
    summary: Option<CompactSummary>,
) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&Envelope {
            schema_version: 2,
            command,
            results,
            summary
        })?
    );
    Ok(())
}
