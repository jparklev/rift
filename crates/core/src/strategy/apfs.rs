use super::Strategy;
use crate::{
    CopyMode, Error, Result,
    filter::{CopyFilter, is_git_metadata},
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct ApfsStrategy;

impl Strategy for ApfsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode) -> Result<()> {
        match mode {
            CopyMode::All => clone_path_apfs(from, to),
            CopyMode::Filtered => clone_filtered_directory_apfs(from, to),
        }
    }
}

/// Filtered clones clone maximal clean subtrees with a single `clonefile`
/// each, which preserves modes, timestamps, and xattrs in one syscall, and
/// fall back to per-entry cloning only inside directories that contain an
/// excluded path. Hard links are preserved between per-entry cloned files;
/// links into a wholesale-cloned subtree become independent clones, matching
/// `CopyMode::All` semantics (data blocks stay shared with the source).
fn clone_filtered_directory_apfs(from: &Path, to: &Path) -> Result<()> {
    let filter = CopyFilter::for_source(from);
    let mut dirty = HashSet::new();
    if !scan_directory_apfs(from, Path::new(""), &filter, &mut dirty)? {
        // A filtered clone with no exclusions has exactly CopyMode::All's
        // observable file-tree semantics. Avoid a second traversal and many
        // individual clonefile calls on the overwhelmingly common clean path.
        return clone_path_apfs(from, to);
    }
    fs::create_dir(to)?;
    let mut hard_links = HashMap::new();
    clone_children_apfs(from, to, Path::new(""), &dirty, &filter, &mut hard_links)?;
    copy_metadata_apfs(from, to, MetadataTarget::FileOrDirectory)
}

/// Record in `dirty` every relative directory with an excluded descendant;
/// such directories cannot be cloned wholesale. Excluded directories are not
/// entered, so this readdir-only pass never descends into dropped artifacts.
fn scan_directory_apfs(
    root: &Path,
    rel: &Path,
    filter: &CopyFilter,
    dirty: &mut HashSet<PathBuf>,
) -> Result<bool> {
    let mut is_dirty = false;
    for entry in fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let rel_child = rel.join(entry.file_name());
        let child_is_dirty = filter.excludes(&rel_child)
            || (entry.file_type()?.is_dir()
                && !is_git_metadata(&rel_child)
                && scan_directory_apfs(root, &rel_child, filter, dirty)?);
        if child_is_dirty {
            is_dirty = true;
        }
    }
    if is_dirty {
        dirty.insert(rel.to_path_buf());
    }
    Ok(is_dirty)
}

fn clone_children_apfs(
    from: &Path,
    to: &Path,
    rel: &Path,
    dirty: &HashSet<PathBuf>,
    filter: &CopyFilter,
    hard_links: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    for entry in fs::read_dir(from.join(rel))? {
        let entry = entry?;
        let rel_child = rel.join(entry.file_name());
        if filter.excludes(&rel_child) {
            continue;
        }
        let source = from.join(&rel_child);
        let destination = to.join(&rel_child);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if dirty.contains(&rel_child) {
                fs::create_dir(&destination)?;
                clone_children_apfs(from, to, &rel_child, dirty, filter, hard_links)?;
                copy_metadata_apfs(&source, &destination, MetadataTarget::FileOrDirectory)?;
            } else {
                clone_path_apfs(&source, &destination)?;
            }
        } else if file_type.is_file() {
            let metadata = fs::symlink_metadata(&source)?;
            let key = (metadata.dev(), metadata.ino());
            if metadata.nlink() > 1 {
                if let Some(existing) = hard_links.get(&key) {
                    fs::hard_link(existing, &destination)?;
                } else {
                    clone_path_apfs(&source, &destination)?;
                    hard_links.insert(key, destination.clone());
                }
            } else {
                clone_path_apfs(&source, &destination)?;
            }
            copy_metadata_apfs(&source, &destination, MetadataTarget::FileOrDirectory)?;
        } else if file_type.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&source)?, &destination)?;
            copy_metadata_apfs(&source, &destination, MetadataTarget::Symlink)?;
        } else {
            return Err(Error::UnsupportedEntry(source));
        }
    }
    Ok(())
}

fn clone_path_apfs(from: &Path, to: &Path) -> Result<()> {
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

#[derive(Clone, Copy)]
enum MetadataTarget {
    FileOrDirectory,
    Symlink,
}

fn copy_metadata_apfs(from: &Path, to: &Path, target: MetadataTarget) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(from)?;
    let destination = c_path(to)?;
    // SAFETY: `destination` is a valid null-terminated path, and uid/gid come
    // from filesystem metadata for `from`.
    if unsafe { libc::lchown(destination.as_ptr(), metadata.uid(), metadata.gid()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // `clonefile` preserves the source mode, which may be read-only (Git loose
    // objects are 0444). Stamping xattrs and timestamps onto a read-only file
    // fails with EACCES on macOS, so widen the mode while writing metadata and
    // apply the authoritative (possibly read-only) mode last. The transient
    // widen carries only permission bits — never setuid/setgid/sticky.
    if matches!(target, MetadataTarget::FileOrDirectory) {
        fs::set_permissions(
            to,
            fs::Permissions::from_mode((metadata.mode() & 0o777) | 0o200),
        )?;
    }
    copy_xattrs_apfs(from, to)?;
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    // SAFETY: `destination` is a live C string and `times` contains exactly the
    // two timestamps expected by `utimensat`.
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            destination.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if matches!(target, MetadataTarget::FileOrDirectory) {
        fs::set_permissions(to, fs::Permissions::from_mode(metadata.mode()))?;
    }
    Ok(())
}

fn copy_xattrs_apfs(from: &Path, to: &Path) -> Result<()> {
    let from = c_path(from)?;
    let to = c_path(to)?;
    // SAFETY: `from` is a valid C path. A null buffer with size 0 asks the
    // kernel for the required list size.
    let size =
        unsafe { libc::listxattr(from.as_ptr(), std::ptr::null_mut(), 0, libc::XATTR_NOFOLLOW) };
    if size < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = vec![0_u8; size as usize];
    // SAFETY: `names` was allocated with the size reported by the previous
    // `listxattr` call, and its pointer is valid for writes of that length.
    if size > 0
        && unsafe {
            libc::listxattr(
                from.as_ptr(),
                names.as_mut_ptr().cast(),
                names.len(),
                libc::XATTR_NOFOLLOW,
            )
        } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::ffi::CString::new(name)
            .map_err(|_| Error::Path("extended attribute name contains a null byte".into()))?;
        // SAFETY: `from` and `name` are valid C strings. A null buffer with
        // size 0 asks the kernel for this attribute's value length.
        let size = unsafe {
            libc::getxattr(
                from.as_ptr(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
                0,
                libc::XATTR_NOFOLLOW,
            )
        };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut value = vec![0_u8; size as usize];
        // SAFETY: `value` was allocated with the exact size reported by
        // `getxattr`, and the path and attribute name are valid C strings.
        if size > 0
            && unsafe {
                libc::getxattr(
                    from.as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: `to`, `name`, and `value` are valid for the duration of the
        // call. `XATTR_NOFOLLOW` keeps symlink behavior consistent.
        if unsafe {
            libc::setxattr(
                to.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

fn c_path(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::TempDir;

    #[test]
    fn git_metadata_is_not_scanned_as_filtered_content() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join(".git/refs/heads/build")).unwrap();
        fs::write(source.join(".git/refs/heads/build/test"), "ref").unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "dep").unwrap();
        let filter = CopyFilter::for_source(&source);
        let mut dirty = HashSet::new();

        assert!(scan_directory_apfs(&source, Path::new(""), &filter, &mut dirty).unwrap());
        assert!(!dirty.contains(Path::new(".git")));
        assert!(!dirty.contains(Path::new(".git/refs/heads/build")));
    }

    #[test]
    fn clean_filtered_tree_has_no_dirty_directories() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("src/main.rs"), "fn main() {}\n").unwrap();
        let filter = CopyFilter::for_source(&source);
        let mut dirty = HashSet::new();

        assert!(!scan_directory_apfs(&source, Path::new(""), &filter, &mut dirty).unwrap());
        assert!(dirty.is_empty());
    }

    #[test]
    fn strategy_clones_and_removes_a_workspace() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "hello").unwrap();
        let strategy = ApfsStrategy;

        strategy
            .copy_directory(&source, &destination, CopyMode::All)
            .unwrap();
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
            assert!(
                ApfsStrategy
                    .copy_directory(&source, &destination, CopyMode::All)
                    .is_ok()
            );
        }
    }

    #[test]
    fn filtered_clone_keeps_git_tracked_artifacts_but_drops_untracked_ones() {
        use std::process::Command;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .arg("init")
                .arg("--quiet")
                .status()
                .unwrap()
                .success()
        );
        // A repo that *commits* its build output: `dist/` is normally filtered
        // by name, but a clone that drops a tracked file would surface it as a
        // spurious deletion in any later diff.
        fs::create_dir(source.join("dist")).unwrap();
        fs::write(source.join("dist/keep.txt"), "tracked build output").unwrap();
        fs::write(source.join("dist/scratch.txt"), "untracked artifact").unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "dep").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(["add", "dist/keep.txt"])
                .status()
                .unwrap()
                .success()
        );

        ApfsStrategy
            .copy_directory(&source, &destination, CopyMode::Filtered)
            .unwrap();

        // Tracked file survives even though its directory matches the filter.
        assert_eq!(
            fs::read_to_string(destination.join("dist/keep.txt")).unwrap(),
            "tracked build output"
        );
        // Untracked artifacts inside the same directory are still dropped.
        assert!(!destination.join("dist/scratch.txt").exists());
        // A fully-untracked excluded directory is pruned entirely.
        assert!(!destination.join("node_modules").exists());
    }

    #[test]
    fn filtered_strategy_preserves_included_metadata_and_hard_links() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let nested = source.join("nested");
        fs::create_dir(&source).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o710)).unwrap();
        let file = nested.join("file.txt");
        fs::write(&file, "hello").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(&file, nested.join("hard.txt")).unwrap();
        std::os::unix::fs::symlink("file.txt", nested.join("link.txt")).unwrap();
        // An exclusion inside `nested` keeps it on the per-entry path, which
        // is the one that guarantees hard-link preservation.
        fs::create_dir_all(nested.join("node_modules/pkg")).unwrap();
        fs::write(nested.join("node_modules/pkg/index.js"), "dep").unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();

        ApfsStrategy
            .copy_directory(&source, &destination, CopyMode::Filtered)
            .unwrap();

        assert!(!destination.join("node_modules").exists());
        assert!(!destination.join("nested/node_modules").exists());
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
            fs::metadata(destination.join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o710
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[test]
    fn filtered_clone_of_clean_subtree_preserves_content_and_metadata() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let clean = source.join("clean/deep");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(&clean).unwrap();
        fs::set_permissions(&clean, fs::Permissions::from_mode(0o700)).unwrap();
        let file = clean.join("file.txt");
        fs::write(&file, "kept").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::hard_link(&file, clean.join("hard.txt")).unwrap();
        std::os::unix::fs::symlink("file.txt", clean.join("link.txt")).unwrap();
        // The root is dirty (an exclusion lives beside `clean`), but `clean`
        // itself contains none, so it is cloned wholesale.
        fs::create_dir(source.join("node_modules")).unwrap();
        fs::write(source.join("node_modules/index.js"), "dep").unwrap();

        ApfsStrategy
            .copy_directory(&source, &destination, CopyMode::Filtered)
            .unwrap();

        let cloned = destination.join("clean/deep");
        assert!(!destination.join("node_modules").exists());
        assert_eq!(fs::read_to_string(cloned.join("file.txt")).unwrap(), "kept");
        assert_eq!(
            fs::read_link(cloned.join("link.txt")).unwrap(),
            Path::new("file.txt")
        );
        assert_eq!(
            fs::metadata(cloned.join("file.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(&cloned).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Wholesale-cloned subtrees follow `clonefile` semantics: hard links
        // become independent clones with identical content (blocks stay
        // shared with the source until either copy is rewritten).
        assert_eq!(fs::read_to_string(cloned.join("hard.txt")).unwrap(), "kept");
        // Clones diverge from the source like any other rift.
        fs::write(cloned.join("file.txt"), "rewritten").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "kept");
    }
}
