use crate::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(test)]
use walkdir::WalkDir;

pub(crate) trait CopyStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()>;

    fn initialize_directory(&self, _path: &Path) -> Result<Option<PathBuf>> {
        Ok(None)
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        fs::remove_dir_all(path)?;
        Ok(())
    }
}

pub(crate) struct CowStrategy;

impl CopyStrategy for CowStrategy {
    fn copy_directory(&self, from: &Path, to: &Path) -> Result<()> {
        #[cfg(target_os = "linux")]
        return copy_directory_linux(from, to);

        #[cfg(target_os = "macos")]
        return copy_directory_macos(from, to);

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (from, to);
            Err(Error::CowUnavailable(
                "no copy-on-write strategy has been implemented for this platform".into(),
            ))
        }
    }

    fn initialize_directory(&self, path: &Path) -> Result<Option<PathBuf>> {
        #[cfg(target_os = "linux")]
        return initialize_directory_linux(path);

        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(Error::CowUnavailable(
                "rift init is currently implemented only for btrfs on Linux".into(),
            ))
        }
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        #[cfg(target_os = "linux")]
        return remove_directory_linux(path);

        #[cfg(not(target_os = "linux"))]
        {
            fs::remove_dir_all(path)?;
            Ok(())
        }
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
fn initialize_directory_linux(path: &Path) -> Result<Option<PathBuf>> {
    if !is_btrfs_filesystem(path)? {
        return Err(Error::CowUnavailable(format!(
            "{} is not on a btrfs filesystem",
            path.display()
        )));
    }
    if is_btrfs_subvolume(path)? {
        return Ok(None);
    }

    let parent = path
        .parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", path.display())))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", path.display())))?;
    let mut backup_name = file_name.to_os_string();
    backup_name.push(".rift-backup");
    let backup = parent.join(backup_name);
    if backup.exists() {
        return Err(Error::AlreadyExists(backup));
    }
    let staging = parent.join(format!(".rift-init-{}", ulid::Ulid::new()));

    create_btrfs_subvolume(&staging)?;

    let result = (|| {
        let source_contents = path.join(".");
        let output = Command::new("cp")
            .args(["-a", "--reflink=always", "--"])
            .arg(source_contents)
            .arg(&staging)
            .output()?;
        if !output.status.success() {
            return Err(Error::CowUnavailable(format!(
                "failed to import {} into a btrfs subvolume: {}",
                path.display(),
                command_error(&output)
            )));
        }
        fs::set_permissions(&staging, fs::metadata(path)?.permissions())?;
        fs::rename(path, &backup)?;
        if let Err(error) = fs::rename(&staging, path) {
            let _ = fs::rename(&backup, path);
            return Err(error.into());
        }
        Ok(Some(backup.clone()))
    })();
    if result.is_err() && staging.exists() {
        let _ = remove_directory_linux(&staging);
    }
    result
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

#[cfg(target_os = "linux")]
fn command_error(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
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
impl CopyStrategy for TestStrategy {
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
impl CopyStrategy for FailureStrategy {
    fn copy_directory(&self, _from: &Path, _to: &Path) -> Result<()> {
        Err(Error::CowUnavailable("test failure".into()))
    }
}
