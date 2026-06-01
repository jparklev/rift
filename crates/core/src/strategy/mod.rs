#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
use crate::Error;
use crate::{InitProgress, Result};
use std::fs;
use std::path::Path;

#[cfg(target_os = "macos")]
mod apfs;
#[cfg(target_os = "linux")]
mod btrfs;

pub(crate) trait Strategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()>;

    fn initialize_directory(
        &self,
        _path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
    ) -> Result<StrategyInit> {
        Ok(StrategyInit::AlreadyNative)
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        fs::remove_dir_all(path)?;
        Ok(())
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StrategyInit {
    AlreadyNative,
    Converted,
}

pub(crate) fn default_strategy() -> Box<dyn Strategy> {
    #[cfg(target_os = "linux")]
    return Box::new(btrfs::BtrfsStrategy);

    #[cfg(target_os = "macos")]
    return Box::new(apfs::ApfsStrategy);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Box::new(UnsupportedStrategy);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct UnsupportedStrategy;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Strategy for UnsupportedStrategy {
    fn copy_directory(&self, _from: &Path, _to: &Path) -> Result<()> {
        Err(Error::CowUnavailable(
            "no copy-on-write strategy has been implemented for this platform".into(),
        ))
    }
}

#[cfg(all(test, unix))]
fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    Ok(())
}

#[cfg(all(test, windows))]
fn copy_symlink(from: &Path, to: &Path) -> Result<()> {
    let target = fs::read_link(from)?;
    if fs::metadata(from)?.is_dir() {
        std::os::windows::fs::symlink_dir(target, to)?;
        return Ok(());
    }
    std::os::windows::fs::symlink_file(target, to)?;
    Ok(())
}

#[cfg(test)]
pub(crate) struct TestStrategy;

#[cfg(test)]
impl Strategy for TestStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        fs::create_dir(to)?;
        for entry in walkdir::WalkDir::new(from).min_depth(1).follow_links(false) {
            let entry = entry?;
            let destination = to.join(
                entry
                    .path()
                    .strip_prefix(from)
                    .map_err(|error| Error::Path(error.to_string()))?,
            );
            if entry.file_type().is_dir() {
                fs::create_dir(&destination)?;
                continue;
            }
            if entry.file_type().is_symlink() {
                copy_symlink(entry.path(), &destination)?;
                continue;
            }
            fs::copy(entry.path(), destination)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) struct FailureStrategy;

#[cfg(test)]
impl Strategy for FailureStrategy {
    fn copy_directory(&self, _from: &Path, _to: &Path) -> Result<()> {
        Err(Error::CowUnavailable("test failure".into()))
    }
}
