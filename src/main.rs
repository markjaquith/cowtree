mod cli;
mod compact;
mod create;
mod eligibility;
mod error;
mod git;
mod git_worktree;
mod output;
mod platform;
mod receipt;
mod worktree;

use clap::Parser;

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("git")) {
        match git_worktree::run(std::env::args_os().skip(2).collect()) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("cowtree: {error}");
                let code = match error {
                    error::Error::GitProcess { code, .. } => code,
                    _ => 1,
                };
                std::process::exit(code);
            }
        }
    }
    if let Err(error) = cli::run(cli::Cli::parse()) {
        eprintln!("cowtree: {error}");
        std::process::exit(1);
    }
}
