use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
};

use clap::{Args, Parser, Subcommand};

use crate::{
    add, compact,
    error::{Error, Result},
    git,
    output::{self, CompactResult, Envelope, StatusResult, Summary},
    receipt::{self, ReceiptState},
    worktree::{self, Worktree},
};

#[derive(Debug, Parser)]
#[command(
    name = "cowtree",
    version,
    about = "Create and compact Git worktrees with copy-on-write clones"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a copy-on-write worktree
    Add(AddArgs),
    Compact(OperationArgs),
    Status(StatusArgs),
}

#[derive(Debug, Args)]
#[command(disable_help_flag = true, trailing_var_arg = true)]
struct AddArgs {
    /// Arguments accepted by `git worktree add`
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
    args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct OperationArgs {
    /// Branch or registered worktree path
    target: Option<PathBuf>,
    /// Process every linked worktree except the source
    #[arg(long, conflicts_with = "target")]
    all: bool,
    /// Checked-out source branch or worktree path
    #[arg(long)]
    source: Option<PathBuf>,
    /// Emit stable, versioned JSON
    #[arg(long)]
    json: bool,
    /// Calculate candidates without cloning (compact only)
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Branch or registered worktree path (defaults to the current worktree)
    target: Option<PathBuf>,
    #[arg(long, conflicts_with = "target")]
    all: bool,
    #[arg(long)]
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

fn source_for(cwd: &Path, worktrees: &[Worktree], requested: Option<&PathBuf>) -> Result<Worktree> {
    match requested {
        Some(value) => worktree::resolve(worktrees, value).cloned(),
        None => worktree::default_source(worktrees, cwd),
    }
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
    let source = source_for(cwd, worktrees, args.source.as_ref())?;
    compact::validate_source(&source)?;
    let targets = targets_for(
        cwd,
        worktrees,
        args.target.as_ref(),
        args.all,
        Some(&source),
    )?;
    let total = targets.len();
    let mut results = Vec::new();
    let mut summary = Summary::default();
    for (index, target) in targets.into_iter().enumerate() {
        if args.all && compact::is_current_receipt(&source, target) {
            summary.skipped += 1;
            if !args.json {
                println!(
                    "[{}/{}] skipping {} (current receipt)",
                    index + 1,
                    total,
                    target.label()
                );
            }
            results.push(CompactResult {
                worktree: target.path.to_string_lossy().into_owned(),
                label: target.label(),
                outcome: "skipped_current_receipt".into(),
                cloned_files: 0,
                eligible_files: 0,
                eligible_logical_bytes: 0,
                eligible_allocated_bytes: 0,
                skipped_divergent_paths: 0,
                error: None,
            });
            continue;
        }
        if !args.json {
            println!("[{}/{}] compacting {}", index + 1, total, target.label());
        }
        match compact::compact_one(&source, target, args.dry_run) {
            Ok((result, seconds)) => {
                summary.compacted += usize::from(!args.dry_run);
                summary.skipped += usize::from(args.dry_run);
                if !args.json {
                    println!(
                        "[{}/{}] {} {} files ({} eligible) in {:.1}s",
                        index + 1,
                        total,
                        if args.dry_run {
                            "would compact"
                        } else {
                            "compacted"
                        },
                        result
                            .cloned_files
                            .max(result.eligible_files * u64::from(args.dry_run)),
                        output::bytes(result.eligible_allocated_bytes),
                        seconds
                    );
                }
                results.push(result);
            }
            Err(error) => {
                summary.failed += 1;
                if !args.json {
                    eprintln!(
                        "[{}/{}] failed {}: {}",
                        index + 1,
                        total,
                        target.label(),
                        error
                    );
                }
                results.push(CompactResult {
                    worktree: target.path.to_string_lossy().into_owned(),
                    label: target.label(),
                    outcome: "failed".into(),
                    cloned_files: 0,
                    eligible_files: 0,
                    eligible_logical_bytes: 0,
                    eligible_allocated_bytes: 0,
                    skipped_divergent_paths: 0,
                    error: Some(error.to_string()),
                });
                if !args.all {
                    break;
                }
            }
        }
    }
    if args.json {
        print_json("compact", results, summary)?;
    } else {
        println!(
            "complete: {} compacted, {} skipped, {} failed",
            summary.compacted, summary.skipped, summary.failed
        );
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
    let targets = targets_for(cwd, worktrees, args.target.as_ref(), args.all, None)?;
    let mut results = Vec::new();
    for target in targets {
        let located = receipt::read_for(target)?;
        let state = match located.as_ref() {
            None => receipt::creation_state(target),
            Some(Err(_)) => ReceiptState::Invalid,
            Some(Ok(value)) => {
                let reference = format!("{}^{{commit}}", value.receipt.source_branch);
                let source_commit = git::text(cwd, ["rev-parse", "--verify", reference.as_str()]);
                match source_commit {
                    Ok(commit) => {
                        let state = receipt::state(located.as_ref(), &commit, &target.head);
                        if state == ReceiptState::Compacted {
                            let _ = receipt::migrate_legacy(target, value);
                        }
                        state
                    }
                    Err(_) => ReceiptState::Unknown,
                }
            }
        };
        let result = StatusResult {
            worktree: target.path.to_string_lossy().into_owned(),
            label: target.label(),
            state,
        };
        if !args.json {
            println!("{}: {} ({})", state.as_str(), result.label, result.worktree);
        }
        results.push(result);
    }
    if args.json {
        print_json("status", results, Summary::default())?;
    }
    Ok(())
}

fn print_json<T: serde::Serialize>(
    command: &'static str,
    results: Vec<T>,
    summary: Summary,
) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&Envelope {
            schema_version: 1,
            command,
            results,
            summary
        })?
    );
    Ok(())
}
