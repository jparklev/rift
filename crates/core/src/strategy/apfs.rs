use super::Strategy;
use crate::{Error, Result};
use std::path::Path;

pub(super) struct ApfsStrategy;

impl Strategy for ApfsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(from.as_os_str().as_bytes())
            .map_err(|_| Error::Path(format!("path contains a null byte: {}", from.display())))?;
        let destination = CString::new(to.as_os_str().as_bytes())
            .map_err(|_| Error::Path(format!("path contains a null byte: {}", to.display())))?;
        // SAFETY: `source` and `destination` are null-terminated C strings
        // built above, and both live for the duration of the call.
        let result = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) };
        if result == 0 {
            return Ok(());
        }
        Err(Error::CowUnavailable(format!(
            "failed to clone {}: {}",
            from.display(),
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn strategy_clones_and_removes_a_workspace() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();
        let strategy = ApfsStrategy;

        strategy.copy_directory(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "hello"
        );
        strategy.remove_directory(&destination).unwrap();
        assert!(!destination.exists());
    }

    #[test]
    fn integration_environment_is_required_by_ci() {
        if std::env::var_os("RIFT_REQUIRE_APFS_TESTS").is_some() {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            fs::create_dir(&source).unwrap();
            assert!(ApfsStrategy.copy_directory(&source, &destination).is_ok());
        }
    }
}
