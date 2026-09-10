use std::path::Path;

use crate::error::Result;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_create;
#[cfg(target_os = "macos")]
pub use macos::CloneOutcome;

#[cfg(not(target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
pub enum CloneOutcome {
    Cloned,
    ChangedDuringClone,
    NotRegular,
}

pub trait ClonePlatform {
    fn validate(&self, source: &Path, target: &Path) -> Result<()>;
    fn clone_replacing(
        &self,
        source_root: &Path,
        target_root: &Path,
        relative: &Path,
        sequence: u64,
    ) -> Result<CloneOutcome>;
}

pub struct SystemPlatform;

pub fn clone_new(
    source: &Path,
    target: &Path,
    relative: &Path,
    mode: u32,
    verify: impl FnOnce(&mut std::fs::File) -> Result<bool>,
) -> Result<CloneOutcome> {
    #[cfg(target_os = "macos")]
    return macos_create::clone_new(source, target, relative, mode, verify);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (source, target, relative, mode, verify);
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
        sequence: u64,
    ) -> Result<CloneOutcome> {
        #[cfg(target_os = "macos")]
        return macos::clone_replacing(source_root, target_root, relative, sequence);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (source_root, target_root, relative, sequence);
            Err(crate::error::Error::UnsupportedFilesystem(
                "cowtree v1 requires macOS on APFS".into(),
            ))
        }
    }
}
