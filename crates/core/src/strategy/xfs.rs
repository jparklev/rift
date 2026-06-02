use super::linux::{Filesystem, filesystem};
use super::reflink::{clone_directory_linux, reflink_file_linux};
use super::{Strategy, StrategyInit};
use crate::{Error, InitProgress, Result};
use std::fs;
use std::path::Path;

pub(super) struct XfsStrategy;

impl Strategy for XfsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        if !is_xfs_filesystem(from)? {
            return Err(Error::CowUnavailable(format!(
                "{} is not on an XFS filesystem",
                from.display()
            )));
        }
        let destination_parent = to
            .parent()
            .ok_or_else(|| Error::Path(format!("destination has no parent: {}", to.display())))?;
        if !is_xfs_filesystem(destination_parent)?
            || fs::metadata(from)?.dev() != fs::metadata(destination_parent)?.dev()
        {
            return Err(Error::CowUnavailable(format!(
                "XFS reflinks require source and destination on the same filesystem: {}",
                to.display()
            )));
        }
        clone_directory_linux(from, to)
    }

    fn initialize_directory(
        &self,
        path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
    ) -> Result<StrategyInit> {
        if !is_xfs_filesystem(path)? {
            return Err(Error::CowUnavailable(format!(
                "{} is not on an XFS filesystem",
                path.display()
            )));
        }
        verify_reflinks_linux(path)?;
        Ok(StrategyInit::AlreadyNative)
    }
}

fn is_xfs_filesystem(path: &Path) -> Result<bool> {
    Ok(matches!(filesystem(path)?, Filesystem::Xfs))
}

fn verify_reflinks_linux(path: &Path) -> Result<()> {
    let operation_id = ulid::Ulid::new();
    let source = path.join(format!(".rift-reflink-probe-{operation_id}"));
    let destination = path.join(format!(".rift-reflink-probe-copy-{operation_id}"));
    fs::write(&source, b"rift")?;
    let result = reflink_file_linux(&source, &destination);
    let cleanup = [&source, &destination]
        .into_iter()
        .filter(|path| path.exists())
        .try_for_each(fs::remove_file);
    result.and(cleanup.map_err(Error::from))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::linux::LinuxStrategy;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::{Builder, TempDir};

    fn xfs_temp() -> Option<TempDir> {
        let temp = Builder::new()
            .prefix(".rift-core-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        is_xfs_filesystem(temp.path()).unwrap().then_some(temp)
    }

    #[test]
    fn xfs_integration_environment_is_available() {
        if std::env::var_os("RIFT_REQUIRE_XFS_TESTS").is_some() {
            assert!(
                xfs_temp().is_some(),
                "RIFT_REQUIRE_XFS_TESTS requires the checkout filesystem to be XFS"
            );
        }
    }

    #[test]
    fn native_init_verifies_reflink_support() {
        let Some(temp) = xfs_temp() else {
            return;
        };
        assert_eq!(
            LinuxStrategy
                .initialize_directory(temp.path(), &mut |_| {})
                .unwrap(),
            StrategyInit::AlreadyNative
        );
    }

    #[test]
    fn native_copy_preserves_files_links_and_metadata() {
        let Some(temp) = xfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let nested = source.join("nested");
        fs::create_dir(&source).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        fs::create_dir(&nested).unwrap();
        let file = nested.join("file.txt");
        fs::write(&file, "hello").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(&file, nested.join("hard.txt")).unwrap();
        std::os::unix::fs::symlink("file.txt", nested.join("link.txt")).unwrap();

        LinuxStrategy.copy_directory(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_link(destination.join("nested/link.txt")).unwrap(),
            Path::new("file.txt")
        );
        assert_eq!(
            fs::metadata(destination.join("nested/file.txt"))
                .unwrap()
                .ino(),
            fs::metadata(destination.join("nested/hard.txt"))
                .unwrap()
                .ino()
        );
        assert_eq!(
            fs::metadata(destination.join("nested/file.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o750
        );
        LinuxStrategy.remove_directory(&destination).unwrap();
        assert!(!destination.exists());
    }

    #[test]
    fn native_copy_rejects_storage_on_another_filesystem() {
        let Some(temp) = xfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let other = TempDir::new().unwrap();
        if fs::metadata(&source).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
            return;
        }

        assert!(matches!(
            LinuxStrategy.copy_directory(&source, &other.path().join("destination")),
            Err(Error::CowUnavailable(_))
        ));
    }
}
