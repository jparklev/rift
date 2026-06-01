use crate::{Error, InitProgress, Result};
use std::fs;
use std::path::Path;
#[cfg(any(test, target_os = "linux"))]
use walkdir::WalkDir;

pub(crate) trait Strategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()>;

    fn initialize_directory(
        &self,
        _path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
    ) -> Result<bool> {
        Ok(false)
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        fs::remove_dir_all(path)?;
        Ok(())
    }
}

pub(crate) fn default_strategy() -> Box<dyn Strategy> {
    #[cfg(target_os = "linux")]
    return Box::new(BtrfsStrategy);

    #[cfg(target_os = "macos")]
    return Box::new(ApfsStrategy);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Box::new(UnsupportedStrategy);
}

#[cfg(target_os = "linux")]
struct BtrfsStrategy;

#[cfg(target_os = "linux")]
impl Strategy for BtrfsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        copy_directory_linux(from, to)
    }

    fn initialize_directory(
        &self,
        path: &Path,
        progress: &mut dyn FnMut(InitProgress),
    ) -> Result<bool> {
        initialize_directory_linux(path, progress)
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        remove_directory_linux(path)
    }
}

#[cfg(target_os = "macos")]
struct ApfsStrategy;

#[cfg(target_os = "macos")]
impl Strategy for ApfsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        copy_directory_macos(from, to)
    }
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

#[cfg(target_os = "macos")]
fn copy_directory_macos(from: &Path, to: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", from.display())))?;
    let destination = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", to.display())))?;
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

#[cfg(target_os = "linux")]
fn copy_directory_linux(from: &Path, to: &Path) -> Result<()> {
    if !is_btrfs_filesystem(from)? {
        return Err(Error::CowUnavailable(format!(
            "Linux snapshot creation requires btrfs; {} is on another filesystem",
            from.display()
        )));
    }
    if !is_btrfs_subvolume(from)? {
        return Err(Error::InitializationRequired(from.to_path_buf()));
    }
    create_btrfs_snapshot(from, to)
}

#[cfg(target_os = "linux")]
fn initialize_directory_linux(path: &Path, progress: &mut dyn FnMut(InitProgress)) -> Result<bool> {
    if !is_btrfs_filesystem(path)? {
        return Err(Error::CowUnavailable(format!(
            "{} is not on a btrfs filesystem",
            path.display()
        )));
    }
    if is_btrfs_subvolume(path)? {
        return Ok(false);
    }

    let parent = path
        .parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", path.display())))?;
    let operation_id = ulid::Ulid::new();
    let staging = parent.join(format!(".rift-init-{operation_id}"));
    let original = parent.join(format!(".rift-init-original-{operation_id}"));

    progress(InitProgress::CreatingSubvolume);
    create_btrfs_subvolume(&staging)?;

    let result = (|| {
        progress(InitProgress::ImportingWorkspace);
        import_directory_linux(path, &staging, progress)?;
        progress(InitProgress::ActivatingWorkspace);
        fs::rename(path, &original).map_err(|error| {
            Error::CowUnavailable(format!(
                "failed to move original workspace aside for activation: {error}"
            ))
        })?;
        if let Err(error) = fs::rename(&staging, path) {
            return match fs::rename(&original, path) {
                Ok(()) => Err(Error::CowUnavailable(format!(
                    "failed to activate initialized workspace; restored the original workspace: {error}"
                ))),
                Err(rollback) => Err(Error::CowUnavailable(format!(
                    "failed to activate initialized workspace: {error}; also failed to restore the original workspace: {rollback}"
                ))),
            };
        }
        copy_metadata_linux(&original, path, false)?;
        progress(InitProgress::RemovingOriginal);
        fs::remove_dir_all(&original).map_err(|error| {
            Error::CowUnavailable(format!(
                "initialized workspace is active but failed to remove the original directory: {error}"
            ))
        })?;
        Ok(true)
    })();
    if result.is_err() && staging.exists() {
        let _ = remove_directory_linux(&staging);
    }
    result
}

#[cfg(target_os = "linux")]
fn import_directory_linux(
    from: &Path,
    to: &Path,
    progress: &mut dyn FnMut(InitProgress),
) -> Result<()> {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;

    let mut hard_links = HashMap::new();
    let mut directories = Vec::new();
    let mut entries = 0;
    for entry in WalkDir::new(from).min_depth(1).follow_links(false) {
        let entry = entry?;
        let source = entry.path();
        let destination = to.join(
            source
                .strip_prefix(from)
                .map_err(|error| Error::Path(error.to_string()))?,
        );
        let metadata = fs::symlink_metadata(source)?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            fs::create_dir(&destination)?;
            directories.push((source.to_path_buf(), destination));
        } else if file_type.is_file() {
            let key = (metadata.dev(), metadata.ino());
            if metadata.nlink() > 1 {
                if let Some(existing) = hard_links.get(&key) {
                    fs::hard_link(existing, &destination)?;
                } else {
                    reflink_file_linux(source, &destination)?;
                    hard_links.insert(key, destination.clone());
                }
            } else {
                reflink_file_linux(source, &destination)?;
            }
            copy_metadata_linux(source, &destination, false)?;
        } else if file_type.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(source)?, &destination)?;
            copy_metadata_linux(source, &destination, true)?;
        } else {
            return Err(Error::UnsupportedEntry(source.to_path_buf()));
        }
        entries += 1;
        progress(InitProgress::ImportedEntries { entries });
    }
    for (source, destination) in directories.into_iter().rev() {
        copy_metadata_linux(&source, &destination, false)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reflink_file_linux(from: &Path, to: &Path) -> Result<()> {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;

    const FICLONE: libc::c_ulong = 0x4004_9409;
    let source = File::open(from)?;
    let destination = OpenOptions::new().write(true).create_new(true).open(to)?;
    if unsafe { libc::ioctl(destination.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(());
    }
    Err(Error::CowUnavailable(format!(
        "failed to reflink {}: {}",
        from.display(),
        std::io::Error::last_os_error()
    )))
}

#[cfg(target_os = "linux")]
fn copy_metadata_linux(from: &Path, to: &Path, symlink: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(from)?;
    let destination = c_path(to)?;
    if unsafe { libc::lchown(destination.as_ptr(), metadata.uid(), metadata.gid()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if !symlink {
        fs::set_permissions(to, fs::Permissions::from_mode(metadata.mode()))?;
    }
    copy_xattrs_linux(from, to)?;
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
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_xattrs_linux(from: &Path, to: &Path) -> Result<()> {
    let from = c_path(from)?;
    let to = c_path(to)?;
    let size = unsafe { libc::llistxattr(from.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = vec![0_u8; size as usize];
    if size > 0
        && unsafe { libc::llistxattr(from.as_ptr(), names.as_mut_ptr().cast(), names.len()) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::ffi::CString::new(name)
            .map_err(|_| Error::Path("extended attribute name contains a null byte".into()))?;
        let size =
            unsafe { libc::lgetxattr(from.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut value = vec![0_u8; size as usize];
        if size > 0
            && unsafe {
                libc::lgetxattr(
                    from.as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe {
            libc::lsetxattr(
                to.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn c_path(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))
}

#[cfg(target_os = "linux")]
fn remove_directory_linux(path: &Path) -> Result<()> {
    if !is_btrfs_subvolume(path)? {
        fs::remove_dir_all(path)?;
        return Ok(());
    }
    delete_btrfs_subvolume(path)
}

#[cfg(target_os = "linux")]
fn is_btrfs_subvolume(path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    if !is_btrfs_filesystem(path)? {
        return Ok(false);
    }
    Ok(fs::metadata(path)?.ino() == 256)
}

#[cfg(target_os = "linux")]
fn is_btrfs_filesystem(path: &Path) -> Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const BTRFS_SUPER_MAGIC: libc::c_long = 0x9123_683e;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(stat.f_type == BTRFS_SUPER_MAGIC)
}

#[cfg(target_os = "linux")]
fn create_btrfs_subvolume(path: &Path) -> Result<()> {
    btrfs_path_ioctl(path, BTRFS_IOC_SUBVOL_CREATE, None, "create subvolume")
}

#[cfg(target_os = "linux")]
fn create_btrfs_snapshot(from: &Path, to: &Path) -> Result<()> {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    let source = File::open(from)?;
    btrfs_path_ioctl(
        to,
        BTRFS_IOC_SNAP_CREATE,
        Some(source.as_raw_fd()),
        "snapshot",
    )
}

#[cfg(target_os = "linux")]
fn delete_btrfs_subvolume(path: &Path) -> Result<()> {
    match btrfs_path_ioctl(path, BTRFS_IOC_SNAP_DESTROY, None, "delete subvolume") {
        Ok(()) => Ok(()),
        Err(Error::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            remove_emptyable_subvolume(path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn remove_emptyable_subvolume(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry_path)?;
        } else {
            fs::remove_file(entry_path)?;
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}

#[cfg(target_os = "linux")]
const BTRFS_IOC_SNAP_CREATE: libc::c_ulong = 0x5000_9401;
#[cfg(target_os = "linux")]
const BTRFS_IOC_SUBVOL_CREATE: libc::c_ulong = 0x5000_940e;
#[cfg(target_os = "linux")]
const BTRFS_IOC_SNAP_DESTROY: libc::c_ulong = 0x5000_940f;

#[cfg(target_os = "linux")]
#[repr(C)]
struct BtrfsIoctlVolArgs {
    fd: i64,
    name: [libc::c_char; 4088],
}

#[cfg(target_os = "linux")]
fn btrfs_path_ioctl(
    path: &Path,
    request: libc::c_ulong,
    source_fd: Option<libc::c_int>,
    action: &str,
) -> Result<()> {
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let parent = path
        .parent()
        .ok_or_else(|| Error::Path(format!("path has no parent: {}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("path has no name: {}", path.display())))?
        .as_bytes();
    if name.is_empty() || name.len() >= 4088 || name.contains(&0) {
        return Err(Error::Path(format!(
            "invalid btrfs subvolume name: {}",
            path.display()
        )));
    }
    let mut args = BtrfsIoctlVolArgs {
        fd: source_fd.map_or(0, i64::from),
        name: [0; 4088],
    };
    for (destination, byte) in args.name.iter_mut().zip(name) {
        *destination = *byte as libc::c_char;
    }
    let parent = File::open(parent)?;
    let result = unsafe { libc::ioctl(parent.as_raw_fd(), request, &args) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if request == BTRFS_IOC_SNAP_DESTROY {
        return Err(Error::Io(error));
    }
    Err(Error::CowUnavailable(format!(
        "failed to {action} {}: {error}",
        path.display()
    )))
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
        for entry in WalkDir::new(from).min_depth(1).follow_links(false) {
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
