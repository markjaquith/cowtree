mod add;
mod cli;
mod compact;
mod create;
mod eligibility;
mod error;
mod git;
mod output;
mod platform;
mod receipt;
mod worktree;

fn main() {
    match cli::run(cli::Cli::parse()) {
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
