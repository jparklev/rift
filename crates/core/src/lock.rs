use crate::{Result, id::RiftId};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

/// Advisory locks protecting a single provenance root across processes.
///
/// The lock files intentionally live beside the central registry rather than
/// inside a workspace: a root can be renamed, moved to trash, or unregistered
/// while an operation is in flight. Keeping the files is harmless and avoids
/// the split-lock race that deleting a lock pathname would introduce.
pub(crate) struct LockDirectory {
    path: PathBuf,
}

impl LockDirectory {
    pub(crate) fn open(path: PathBuf) -> Result<Self> {
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn lock_root(&self, id: &RiftId) -> Result<LifecycleLock> {
        LifecycleLock::acquire(self.path.join(format!("{}.lock", id.as_str())))
    }

    pub(crate) fn lock_gc(&self) -> Result<LifecycleLock> {
        LifecycleLock::acquire(self.path.join("gc.lock"))
    }

    /// Serialize first-time initialization by canonical workspace path. A root
    /// ID does not exist yet at this point, so this distinct lock closes the
    /// marker/write/registry race between concurrent `rift init` processes.
    pub(crate) fn lock_initialization(&self, path: &Path) -> Result<LifecycleLock> {
        LifecycleLock::acquire(
            self.path
                .join(format!("init-{:016x}.lock", path_hash(path))),
        )
    }
}

fn path_hash(path: &Path) -> u64 {
    // A stable FNV-1a hash makes the lock filename portable and avoids placing
    // arbitrary workspace names in the database-adjacent lock directory.
    // A collision only serializes two otherwise independent initializations;
    // it cannot merge their registry identities or weaken correctness.
    path.to_string_lossy()
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

pub(crate) struct LifecycleLock {
    file: File,
}

impl LifecycleLock {
    fn acquire(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Lock files have no payload; preserve an existing inode so every
            // contender coordinates through the same advisory lock.
            .truncate(false)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the lock as a final fallback, so an
        // unlock failure cannot leave a permanent lock behind.
        let _ = self.file.unlock();
    }
}
