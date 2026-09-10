mod cli;
mod compact;
mod eligibility;
mod error;
mod git;
mod output;
mod platform;
mod receipt;
mod worktree;

use clap::Parser;

fn main() {
    if let Err(error) = cli::run(cli::Cli::parse()) {
        eprintln!("cowtree: {error}");
        std::process::exit(1);
    }
}
