use crate::{Error, Result, id::RiftId};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) fn path(workspace: &Path) -> PathBuf {
    workspace.join(".rift")
}

pub(crate) fn write(workspace: &Path, id: &RiftId) -> Result<()> {
    let marker = path(workspace);
    require_regular_or_missing(&marker)?;

    // Write beside the marker and install it with a rename. On Unix, rename
    // replaces a racing symlink itself rather than following it; on Windows a
    // racing replacement makes the rename fail, which is likewise safe. The
    // temporary name is deliberately unique because marker writes can race
    // across processes before their higher-level lifecycle locks are acquired.
    let temporary = workspace.join(format!(
        ".rift-write-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(format!("{id}\n").as_bytes())?;
        file.sync_data()?;

        // Windows does not replace an existing destination with `rename`.
        // Removing it is safe only after the no-follow type check above; a
        // racing symlink makes the following rename fail rather than redirect
        // this write.
        #[cfg(windows)]
        if symlink_metadata(&marker)?.is_some() {
            remove_regular_path(&marker)?;
        }
        fs::rename(&temporary, &marker)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}

pub(crate) fn read(workspace: &Path) -> Result<Option<RiftId>> {
    let marker = path(workspace);
    if symlink_metadata(&marker)?.is_none() {
        return Ok(None);
    }
    let mut contents = String::new();
    open_regular(&marker)?.read_to_string(&mut contents)?;
    Ok(Some(RiftId::from_stored(contents.trim().to_owned())))
}

/// Remove the workspace marker only when it is a regular file. Absence is
/// intentionally idempotent so interrupted root unregistrations can be
/// recovered safely.
pub(crate) fn remove_regular(workspace: &Path) -> Result<()> {
    let marker = path(workspace);
    if symlink_metadata(&marker)?.is_some() {
        remove_regular_path(&marker)?;
    }
    Ok(())
}

pub(crate) fn verify(workspace: &Path, expected_id: &RiftId) -> Result<()> {
    if read(workspace)?.as_ref() == Some(expected_id) {
        return Ok(());
    }
    Err(Error::MarkerMismatch(workspace.to_path_buf()))
}

fn symlink_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn require_regular_or_missing(path: &Path) -> Result<()> {
    if let Some(metadata) = symlink_metadata(path)?
        && (!metadata.file_type().is_file() || metadata.file_type().is_symlink())
    {
        return Err(Error::UnsafeMarker(path.to_path_buf()));
    }
    Ok(())
}

fn remove_regular_path(path: &Path) -> Result<()> {
    require_regular_or_missing(path)?;
    if symlink_metadata(path)?.is_some() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn open_regular(path: &Path) -> Result<fs::File> {
    require_regular_or_missing(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // `O_NOFOLLOW` closes the metadata/open race on Unix. If an attacker
        // swaps the marker for a symlink, open fails instead of reading an
        // arbitrary external file.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(Error::UnsafeMarker(path.to_path_buf()));
        }
        Ok(file)
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        // Open the reparse point itself so a link swapped in after
        // `symlink_metadata` cannot redirect the marker read to an external
        // target. The handle metadata then identifies the reparse point rather
        // than its target and is rejected below.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(Error::UnsafeMarker(path.to_path_buf()));
        }
        Ok(file)
    }

    #[cfg(not(any(unix, windows)))]
    {
        // Rift has no production filesystem strategy on these platforms. Do
        // not perform a potentially link-following marker read there.
        let _ = path;
        Err(Error::UnsafeMarker(path.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn id() -> RiftId {
        RiftId::from_stored("marker-test".to_owned())
    }

    #[test]
    fn marker_write_replaces_regular_file() {
        let temp = TempDir::new().unwrap();
        fs::write(path(temp.path()), "old\n").unwrap();

        write(temp.path(), &id()).unwrap();

        assert_eq!(read(temp.path()).unwrap(), Some(id()));
        assert!(
            fs::symlink_metadata(path(temp.path()))
                .unwrap()
                .file_type()
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn marker_functions_reject_symlinks_without_following_them() {
        let temp = TempDir::new().unwrap();
        let external = temp.path().join("external");
        std::os::unix::fs::symlink(&external, path(temp.path())).unwrap();

        assert!(matches!(read(temp.path()), Err(Error::UnsafeMarker(_))));
        assert!(matches!(
            write(temp.path(), &id()),
            Err(Error::UnsafeMarker(_))
        ));
        assert!(matches!(
            remove_regular(temp.path()),
            Err(Error::UnsafeMarker(_))
        ));
        assert!(!external.exists());
        assert!(
            fs::symlink_metadata(path(temp.path()))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    fn marker_functions_reject_symlinks_without_following_them() {
        let temp = TempDir::new().unwrap();
        let external = temp.path().join("external");
        match std::os::windows::fs::symlink_file(&external, path(temp.path())) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to create test marker symlink: {error}"),
        }

        assert!(matches!(read(temp.path()), Err(Error::UnsafeMarker(_))));
        assert!(matches!(
            write(temp.path(), &id()),
            Err(Error::UnsafeMarker(_))
        ));
        assert!(!external.exists());
    }

    #[test]
    fn remove_regular_is_idempotent_but_rejects_non_files() {
        let temp = TempDir::new().unwrap();
        remove_regular(temp.path()).unwrap();
        fs::create_dir(path(temp.path())).unwrap();
        assert!(matches!(
            remove_regular(temp.path()),
            Err(Error::UnsafeMarker(_))
        ));
    }
}
