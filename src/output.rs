use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use console::style;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::Serialize;

use crate::receipt::ReceiptState;

#[derive(Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    pub schema_version: u8,
    pub command: &'static str,
    pub results: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<CompactSummary>,
}

#[derive(Default, Clone, Copy, Debug, Serialize)]
pub struct CompactSummary {
    pub compacted: usize,
    pub already_compacted: usize,
    pub dry_run: usize,
    pub failed: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactOutcome {
    Compacted,
    CompactedWithChangedPathsSkipped,
    AlreadyCompacted,
    DryRun,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct CompactResult {
    pub branch: Option<String>,
    pub path: String,
    pub status: ReceiptState,
    pub outcome: CompactOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloned_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eligible_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eligible_logical_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eligible_allocated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_divergent_paths: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_changed_paths: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CompactResult {
    pub fn label(&self) -> &str {
        self.branch.as_deref().unwrap_or(&self.path)
    }
}

pub struct CompactUi {
    progress: Option<ProgressBar>,
}

impl CompactUi {
    pub fn new() -> Self {
        Self { progress: None }
    }

    pub fn start(&mut self, index: usize, total: usize, label: &str, dry_run: bool) {
        let prefix = position(index, total);
        let action = if dry_run { "Inspecting" } else { "Compacting" };
        let message = format!("{} {}", style(action).cyan(), style(label).bold());
        if io::stdout().is_terminal() {
            let progress = ProgressBar::hidden();
            progress.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {prefix:.dim} {msg}")
                    .expect("valid progress style")
                    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
            );
            progress.set_prefix(prefix);
            progress.set_message(message);
            progress.set_draw_target(ProgressDrawTarget::stdout());
            progress.enable_steady_tick(Duration::from_millis(80));
            self.progress = Some(progress);
        } else {
            println!("{prefix} {message}");
        }
    }

    pub fn success(
        &mut self,
        index: usize,
        total: usize,
        result: &CompactResult,
        seconds: f64,
        dry_run: bool,
    ) {
        self.clear();
        let action = if dry_run {
            "would compact"
        } else {
            "compacted"
        };
        let files = if dry_run {
            result.eligible_files.unwrap_or_default()
        } else {
            result.cloned_files.unwrap_or_default()
        };
        let file_word = if files == 1 { "file" } else { "files" };
        println!(
            "{} {} {} {} — {} {} ({} eligible) in {:.1}s",
            style("✓").green().bold(),
            style(position(index, total)).dim(),
            style(action).green().bold(),
            style(result.label()).bold(),
            files,
            file_word,
            bytes(result.eligible_allocated_bytes.unwrap_or_default()),
            seconds
        );
    }

    pub fn already_compacted(&mut self, index: usize, total: usize, label: &str) {
        self.clear();
        println!(
            "{} {} {} — {}",
            style("•").yellow().bold(),
            style(position(index, total)).dim(),
            style(label).bold(),
            style("already compacted").yellow()
        );
    }

    pub fn failed(
        &mut self,
        index: usize,
        total: usize,
        label: &str,
        error: &dyn std::fmt::Display,
    ) {
        self.clear();
        eprintln!(
            "{} {} {} — {}",
            style("✗").red().bold(),
            style(position(index, total)).dim(),
            style(label).bold(),
            style(error).red()
        );
    }

    pub fn summary(&mut self, summary: CompactSummary) {
        self.clear();
        println!(
            "{}  {} compacted · {} already compacted · {} dry-run · {} failed",
            style("Done").cyan().bold(),
            style(summary.compacted).green(),
            style(summary.already_compacted).yellow(),
            style(summary.dry_run).cyan(),
            style(summary.failed).red()
        );
    }

    fn clear(&mut self) {
        if let Some(progress) = self.progress.take() {
            progress.finish_and_clear();
        }
    }
}

impl Drop for CompactUi {
    fn drop(&mut self) {
        self.clear();
    }
}

fn position(index: usize, total: usize) -> String {
    format!("[{index}/{total}]")
}

#[derive(Debug, Serialize)]
pub struct StatusResult {
    pub branch: Option<String>,
    pub path: String,
    pub status: ReceiptState,
}

pub fn print_status(results: &[StatusResult]) {
    for result in results {
        let status = status_label(result.status);
        let (symbol, status) = match result.status {
            ReceiptState::Compacted => (
                style("✓").green().bold().to_string(),
                style(status).green().to_string(),
            ),
            ReceiptState::Created => (
                style("◆").cyan().bold().to_string(),
                style(status).cyan().to_string(),
            ),
            ReceiptState::Stale => (
                style("!").yellow().bold().to_string(),
                style(status).yellow().to_string(),
            ),
            ReceiptState::NotCompacted => (
                style("○").dim().to_string(),
                style(status).dim().to_string(),
            ),
            ReceiptState::Invalid => (
                style("✗").red().bold().to_string(),
                style(status).red().to_string(),
            ),
            ReceiptState::Unknown => (
                style("?").yellow().bold().to_string(),
                style(status).yellow().to_string(),
            ),
        };
        let branch = result.branch.as_deref().unwrap_or("(detached)");
        println!(
            "{} — {}  {symbol} {status}",
            style(branch).bold(),
            style(&result.path).dim()
        );
    }
}

fn status_label(state: ReceiptState) -> &'static str {
    match state {
        ReceiptState::Compacted => "compacted",
        ReceiptState::Created => "created",
        ReceiptState::Stale => "stale",
        ReceiptState::NotCompacted => "not compacted",
        ReceiptState::Invalid => "invalid receipt",
        ReceiptState::Unknown => "unknown",
    }
}

pub fn bytes(value: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.2} {}", UNITS[unit])
    }
}
