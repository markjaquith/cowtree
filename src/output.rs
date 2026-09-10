use serde::Serialize;

use crate::receipt::ReceiptState;

#[derive(Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    pub schema_version: u8,
    pub command: &'static str,
    pub results: Vec<T>,
    pub summary: Summary,
}

#[derive(Default, Clone, Copy, Debug, Serialize)]
pub struct Summary {
    pub compacted: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Serialize)]
pub struct CompactResult {
    pub worktree: String,
    pub label: String,
    pub outcome: String,
    pub cloned_files: u64,
    pub eligible_files: u64,
    pub eligible_logical_bytes: u64,
    pub eligible_allocated_bytes: u64,
    pub skipped_divergent_paths: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusResult {
    pub worktree: String,
    pub label: String,
    pub state: ReceiptState,
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
