use super::{Strategy, StrategyInit, btrfs::BtrfsStrategy, reflink::LinuxReflinkStrategy};
use crate::{Error, InitProgress, Result};
use std::fs;
use std::path::Path;

pub(super) struct LinuxStrategy;

impl Strategy for LinuxStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        match filesystem(from)? {
            Filesystem::Btrfs => BtrfsStrategy.copy_directory(from, to),
            Filesystem::Other => LinuxReflinkStrategy.copy_directory(from, to),
        }
    }

    fn initialize_directory(
        &self,
        path: &Path,
        progress: &mut dyn FnMut(InitProgress),
    ) -> Result<StrategyInit> {
        match filesystem(path)? {
            Filesystem::Btrfs => BtrfsStrategy.initialize_directory(path, progress),
            Filesystem::Other => LinuxReflinkStrategy.initialize_directory(path, progress),
        }
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        match filesystem(path)? {
            Filesystem::Btrfs => BtrfsStrategy.remove_directory(path),
            Filesystem::Other => {
                fs::remove_dir_all(path)?;
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Filesystem {
    Btrfs,
    Other,
}

pub(super) fn filesystem(path: &Path) -> Result<Filesystem> {
    use std::os::unix::ffi::OsStrExt;

    const BTRFS_SUPER_MAGIC: libc::c_long = 0x9123_683e;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))?;
    // SAFETY: `statfs` is a plain C struct; zero initialization is a valid
    // starting state before the kernel fills it.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string, and `stat` points to writable memory
    // for the kernel to initialize.
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(match stat.f_type {
        BTRFS_SUPER_MAGIC => Filesystem::Btrfs,
        _ => Filesystem::Other,
    })
}
