use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::error::Result;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_create;
#[cfg(target_os = "macos")]
pub use macos::CloneOutcome;
#[cfg(target_os = "macos")]
pub use macos_create::CloneDirectoryCache;

#[cfg(not(target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
pub enum CloneOutcome {
    Cloned,
    ChangedDuringClone,
    NotRegular,
}

#[cfg(not(target_os = "macos"))]
pub struct CloneDirectoryCache;

#[cfg(not(target_os = "macos"))]
impl CloneDirectoryCache {
    pub fn new() -> Self {
        Self
    }
}

pub trait ClonePlatform {
    fn validate(&self, source: &Path, target: &Path) -> Result<()>;
    fn clone_replacing(
        &self,
        source_root: &Path,
        target_root: &Path,
        relative: &Path,
        expected_oid: &str,
        sequence: u64,
    ) -> Result<CloneOutcome>;
}

pub struct SystemPlatform;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum ClonePhase {
    SourceLookup,
    SourceMetadata,
    DonorVerification,
    SourceStability,
    TargetLookup,
    CloneFile,
    DestinationLookup,
    RaceValidation,
    MetadataFinalization,
}

const CLONE_PHASES: [(ClonePhase, &str); 9] = [
    (ClonePhase::SourceLookup, "clone source lookup/open"),
    (ClonePhase::SourceMetadata, "clone source metadata/xattrs"),
    (ClonePhase::DonorVerification, "donor hashing"),
    (ClonePhase::SourceStability, "clone preflight stability"),
    (ClonePhase::TargetLookup, "clone target lookup"),
    (ClonePhase::CloneFile, "APFS clone syscall"),
    (
        ClonePhase::DestinationLookup,
        "clone destination open/metadata",
    ),
    (ClonePhase::RaceValidation, "clone postflight validation"),
    (
        ClonePhase::MetadataFinalization,
        "clone permissions/timestamps",
    ),
];

pub struct CloneTimings {
    nanoseconds: [AtomicU64; CLONE_PHASES.len()],
}

impl CloneTimings {
    pub fn new() -> Self {
        Self {
            nanoseconds: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    pub fn record(&self, phase: ClonePhase, duration: Duration) {
        let nanoseconds = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.nanoseconds[phase as usize].fetch_add(nanoseconds, Ordering::Relaxed);
    }

    pub fn summary(&self) -> Vec<(&'static str, Duration)> {
        CLONE_PHASES
            .iter()
            .map(|(phase, label)| {
                (
                    *label,
                    Duration::from_nanos(self.nanoseconds[*phase as usize].load(Ordering::Relaxed)),
                )
            })
            .collect()
    }
}

pub fn cleanup_stale_clones(
    target: &Path,
    tracked: &[PathBuf],
    untracked: &[PathBuf],
) -> Result<u64> {
    #[cfg(target_os = "macos")]
    return macos::cleanup_stale_clones(target, tracked, untracked);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (target, tracked, untracked);
        Ok(0)
    }
}

pub fn clone_new(
    source: &Path,
    target: &Path,
    relative: &Path,
    mode: u32,
    directories: &mut CloneDirectoryCache,
    timings: Option<&CloneTimings>,
    verify: impl FnOnce(&mut std::fs::File) -> Result<bool>,
) -> Result<CloneOutcome> {
    #[cfg(target_os = "macos")]
    return macos_create::clone_new(source, target, relative, mode, directories, timings, verify);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (source, target, relative, mode, directories, timings, verify);
        Err(crate::error::Error::UnsupportedFilesystem(
            "creation requires macOS on APFS".into(),
        ))
    }
}

impl ClonePlatform for SystemPlatform {
    fn validate(&self, source: &Path, target: &Path) -> Result<()> {
        #[cfg(target_os = "macos")]
        return macos::validate_filesystem(source, target);
        #[cfg(not(target_os = "macos"))]
        Err(crate::error::Error::UnsupportedFilesystem(
            "cowtree v1 requires macOS on APFS".into(),
        ))
    }

    fn clone_replacing(
        &self,
        source_root: &Path,
        target_root: &Path,
        relative: &Path,
        expected_oid: &str,
        sequence: u64,
    ) -> Result<CloneOutcome> {
        #[cfg(target_os = "macos")]
        return macos::clone_replacing(source_root, target_root, relative, expected_oid, sequence);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (source_root, target_root, relative, expected_oid, sequence);
            Err(crate::error::Error::UnsupportedFilesystem(
                "cowtree v1 requires macOS on APFS".into(),
            ))
        }
    }
}
