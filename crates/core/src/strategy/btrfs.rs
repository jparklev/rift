use super::linux::{Filesystem, filesystem};
#[cfg(test)]
use super::reflink::c_path;
use super::reflink::{
    MetadataTarget, copy_metadata_linux, import_directory_linux, import_directory_linux_filtered,
};
use super::{Strategy, StrategyInit};
use crate::{CopyMode, Error, InitProgress, Result, filter::CopyFilter};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[cfg(target_os = "linux")]
use std::path::PathBuf;

pub(super) struct BtrfsStrategy;

impl Strategy for BtrfsStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode) -> Result<()> {
        copy_directory_linux(from, to, mode)
    }

    fn initialize_directory(
        &self,
        path: &Path,
        progress: &mut dyn FnMut(InitProgress),
    ) -> Result<StrategyInit> {
        initialize_directory_linux(path, progress)
    }

    fn remove_directory(&self, path: &Path) -> Result<()> {
        remove_directory_linux(path)
    }
}

fn copy_directory_linux(from: &Path, to: &Path, mode: CopyMode) -> Result<()> {
    if !is_btrfs_filesystem(from)? {
        return Err(Error::CowUnavailable(format!(
            "Linux snapshot creation requires btrfs; {} is on another filesystem",
            from.display()
        )));
    }
    if !is_btrfs_subvolume(from)? {
        return Err(Error::InitializationRequired(from.to_path_buf()));
    }
    match mode {
        CopyMode::All => create_btrfs_snapshot(from, to),
        CopyMode::Filtered => create_stable_filtered_btrfs_copy(from, to),
    }
}

#[cfg(target_os = "linux")]
fn create_stable_filtered_btrfs_copy(from: &Path, to: &Path) -> Result<()> {
    // Check filtering and copy boundaries in one directory walk. An excluded
    // directory stops the walk before its descendants, preserving the old
    // dirty-tree fast path for large `node_modules` or `target` trees.
    if !source_allows_filtered_snapshot(from)? {
        return create_filtered_btrfs_subvolume(from, to);
    }

    // Snapshot first, then filter the destination itself. This closes the
    // window where a compiler can create `target/` after the clean source
    // preflight but before Btrfs takes its atomic source image.
    create_btrfs_snapshot(from, to)?;
    let result = (|| {
        // Recheck mount/subvolume boundaries after the snapshot. A nested
        // subvolume created during the first scan is detected while filtering
        // the snapshot as an inode-2 stub.
        if source_has_snapshot_boundary(from)? {
            remove_directory_linux(to)?;
            return create_filtered_btrfs_subvolume(from, to);
        }

        // The snapshot is writable and already shares its retained blocks
        // with the source. Prune only paths the exact CopyFilter excludes;
        // this preserves a clean snapshot's shared metadata and avoids a new
        // hidden staging subvolume that a crash could orphan.
        if prune_filtered_snapshot(to)? {
            remove_directory_linux(to)?;
            return create_filtered_btrfs_subvolume(from, to);
        }
        Ok(())
    })();
    if result.is_err() && to.exists() {
        let _ = remove_directory_linux(to);
    }
    result
}

#[cfg(target_os = "linux")]
fn create_filtered_btrfs_subvolume(from: &Path, to: &Path) -> Result<()> {
    create_btrfs_subvolume(to)?;
    import_directory_linux_filtered(from, to, &mut |_| {})?;
    copy_metadata_linux(from, to, MetadataTarget::FileOrDirectory)
}

#[cfg(target_os = "linux")]
fn initialize_directory_linux(
    path: &Path,
    progress: &mut dyn FnMut(InitProgress),
) -> Result<StrategyInit> {
    ensure_real_directory(path, "workspace")?;
    if let Some(recovered) = recover_pending_initialization(path)? {
        return Ok(recovered);
    }
    if !is_btrfs_filesystem(path)? {
        return Err(Error::CowUnavailable(format!(
            "{} is not on a btrfs filesystem",
            path.display()
        )));
    }
    if is_btrfs_subvolume(path)? {
        return Ok(StrategyInit::AlreadyNative);
    }

    let pending = create_pending_initialization(path)?;
    let preparation = (|| {
        progress(InitProgress::CreatingSubvolume);
        create_btrfs_subvolume(&pending.staging)?;
        progress(InitProgress::ImportingWorkspace);
        import_directory_linux(path, &pending.staging, progress)?;
        // Copy the root metadata before activation. Once the workspace is
        // visible at `path`, every remaining operation is cleanup-only and can
        // be resumed safely on the next `init`.
        copy_metadata_linux(path, &pending.staging, MetadataTarget::FileOrDirectory)?;
        Ok(())
    })();
    if let Err(error) = preparation {
        return fail_pre_activation(path, error);
    }

    progress(InitProgress::ActivatingWorkspace);
    if let Err(error) = rename_exchange(path, &pending.staging) {
        return fail_pre_activation(path, error);
    }
    sync_parent(path)?;

    progress(InitProgress::RemovingOriginal);
    remove_regular_directory(&pending.staging)?;
    remove_pending_initialization(&pending)?;
    Ok(StrategyInit::Converted)
}

#[cfg(target_os = "linux")]
const INIT_STATE_HEADER: &str = "rift-btrfs-init-v1";
#[cfg(target_os = "linux")]
const MAX_INIT_STATE_BYTES: u64 = 1024;

/// A durable marker for the one operation that may be in flight for a given
/// workspace path. It lives beside the workspace rather than inside it, so it
/// survives the exchange that activates the new btrfs subvolume.
#[cfg(target_os = "linux")]
struct PendingInitialization {
    state: PathBuf,
    staging: PathBuf,
}

#[cfg(target_os = "linux")]
fn create_pending_initialization(path: &Path) -> Result<PendingInitialization> {
    let operation = ulid::Ulid::new();
    let pending = pending_initialization(path, operation)?;
    let contents = init_state_contents(path, operation)?;
    write_new_state(&pending.state, &contents)?;
    Ok(pending)
}

#[cfg(target_os = "linux")]
fn pending_initialization(path: &Path, operation: ulid::Ulid) -> Result<PendingInitialization> {
    let parent = workspace_parent(path)?;
    Ok(PendingInitialization {
        state: init_state_path(path)?,
        staging: parent.join(format!(".rift-init-{operation}")),
    })
}

#[cfg(target_os = "linux")]
fn recover_pending_initialization(path: &Path) -> Result<Option<StrategyInit>> {
    let Some(pending) = read_pending_initialization(path)? else {
        return Ok(None);
    };

    ensure_real_directory(path, "workspace")?;
    let workspace_is_subvolume = is_btrfs_subvolume(path)?;
    let staging = directory_entry(&pending.staging, "initialization staging directory")?;

    match (workspace_is_subvolume, staging) {
        // The state was published before the staging subvolume was made, or it
        // was removed after an interrupted import. The original workspace is
        // still in place, so discard the incomplete operation and start over.
        (false, DirectoryEntry::Missing) => {
            remove_pending_initialization(&pending)?;
            Ok(None)
        }
        // The exchange has not happened yet. A staged subvolume is safe to
        // remove because the original workspace remains at `path`.
        (false, DirectoryEntry::Directory) if is_btrfs_subvolume(&pending.staging)? => {
            remove_directory_linux(&pending.staging)?;
            remove_pending_initialization(&pending)?;
            Ok(None)
        }
        // `renameat2(RENAME_EXCHANGE)` has already made the btrfs subvolume
        // live. The staging path now contains the former ordinary directory;
        // complete that cleanup before declaring the conversion recovered.
        (true, DirectoryEntry::Missing) => {
            remove_pending_initialization(&pending)?;
            Ok(Some(StrategyInit::Converted))
        }
        (true, DirectoryEntry::Directory) if !is_btrfs_subvolume(&pending.staging)? => {
            remove_regular_directory(&pending.staging)?;
            remove_pending_initialization(&pending)?;
            Ok(Some(StrategyInit::Converted))
        }
        (false, DirectoryEntry::Directory) => Err(invalid_initialization_state(
            path,
            "the staging path is not a btrfs subvolume before activation",
        )),
        (true, DirectoryEntry::Directory) => Err(invalid_initialization_state(
            path,
            "the staging path is unexpectedly still a btrfs subvolume after activation",
        )),
    }
}

#[cfg(target_os = "linux")]
fn fail_pre_activation(path: &Path, error: Error) -> Result<StrategyInit> {
    match recover_pending_initialization(path) {
        Ok(None) => Err(error),
        Ok(Some(_)) => Err(Error::CowUnavailable(format!(
            "{error}; initialization unexpectedly activated while cleaning up"
        ))),
        Err(recovery) => Err(Error::CowUnavailable(format!(
            "{error}; initialization cleanup requires recovery: {recovery}"
        ))),
    }
}

#[cfg(target_os = "linux")]
fn init_state_path(path: &Path) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;

    let name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", path.display())))?;
    let hash = name
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    Ok(workspace_parent(path)?.join(format!(".rift-init-state-{hash:016x}")))
}

#[cfg(target_os = "linux")]
fn workspace_parent(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", path.display())))
}

#[cfg(target_os = "linux")]
fn init_state_contents(path: &Path, operation: ulid::Ulid) -> Result<String> {
    use std::os::unix::ffi::OsStrExt;

    let name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", path.display())))?;
    let mut target = String::with_capacity(name.as_bytes().len() * 2);
    for byte in name.as_bytes() {
        use std::fmt::Write;

        write!(&mut target, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(format!(
        "{INIT_STATE_HEADER}\ntarget={target}\noperation={operation}\n"
    ))
}

#[cfg(target_os = "linux")]
fn read_pending_initialization(path: &Path) -> Result<Option<PendingInitialization>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let state = init_state_path(path)?;
    let metadata = match secure_state_metadata(&state) {
        Ok(metadata) => metadata,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&state)?;
    let opened = file.metadata()?;
    ensure_secure_state_metadata(&state, &opened)?;
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
            return Err(invalid_initialization_state(
                path,
                "the sidecar changed while it was being opened",
            ));
        }
    }
    let mut contents = String::new();
    file.by_ref()
        .take(MAX_INIT_STATE_BYTES + 1)
        .read_to_string(&mut contents)?;
    if contents.len() as u64 > MAX_INIT_STATE_BYTES {
        return Err(invalid_initialization_state(
            path,
            "the sidecar is too large",
        ));
    }
    let operation = parse_init_state(path, &contents)?;
    Ok(Some(pending_initialization(path, operation)?))
}

#[cfg(target_os = "linux")]
fn parse_init_state(path: &Path, contents: &str) -> Result<ulid::Ulid> {
    use std::os::unix::ffi::OsStrExt;

    let mut lines = contents.lines();
    let header = lines.next();
    let target = lines.next().and_then(|line| line.strip_prefix("target="));
    let operation = lines
        .next()
        .and_then(|line| line.strip_prefix("operation="));
    if header != Some(INIT_STATE_HEADER) || lines.next().is_some() {
        return Err(invalid_initialization_state(
            path,
            "the sidecar format is invalid",
        ));
    }
    let target =
        target.ok_or_else(|| invalid_initialization_state(path, "the target is missing"))?;
    let expected = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", path.display())))?
        .as_bytes();
    if decode_hex(target)? != expected {
        return Err(invalid_initialization_state(
            path,
            "the sidecar belongs to another workspace",
        ));
    }
    operation
        .ok_or_else(|| invalid_initialization_state(path, "the operation is missing"))?
        .parse()
        .map_err(|_| invalid_initialization_state(path, "the operation identifier is invalid"))
}

#[cfg(target_os = "linux")]
fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if value.is_empty() || value.len() % 2 != 0 {
        return Err(Error::CowUnavailable(
            "invalid btrfs initialization state encoding".into(),
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|bytes| {
            let high = hex_digit(bytes[0])?;
            let low = hex_digit(bytes[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn hex_digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(Error::CowUnavailable(
            "invalid btrfs initialization state encoding".into(),
        )),
    }
}

#[cfg(target_os = "linux")]
fn write_new_state(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = workspace_parent(path)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("state has no name: {}", path.display())))?;
    let temporary = parent.join(format!(
        ".{}-tmp-{}",
        file_name.to_string_lossy(),
        ulid::Ulid::new()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let write_result = (|| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        rename_no_replace(&temporary, path)?;
        sync_parent(path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(target_os = "linux")]
fn remove_pending_initialization(pending: &PendingInitialization) -> Result<()> {
    secure_state_metadata(&pending.state)?;
    fs::remove_file(&pending.state)?;
    sync_parent(&pending.state)
}

#[cfg(target_os = "linux")]
fn secure_state_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure_secure_state_metadata(path, &metadata)?;
    Ok(metadata)
}

#[cfg(target_os = "linux")]
fn ensure_secure_state_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::CowUnavailable(format!(
            "refusing unsafe btrfs initialization sidecar: {}",
            path.display()
        )));
    }
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(Error::CowUnavailable(format!(
            "refusing insecure btrfs initialization sidecar: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn sync_parent(path: &Path) -> Result<()> {
    use std::fs::File;

    File::open(workspace_parent(path)?)?.sync_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_exchange(from: &Path, to: &Path) -> Result<()> {
    renameat2(
        from,
        to,
        libc::RENAME_EXCHANGE,
        "atomically exchange workspace and staging",
    )
}

#[cfg(target_os = "linux")]
fn rename_no_replace(from: &Path, to: &Path) -> Result<()> {
    renameat2(
        from,
        to,
        libc::RENAME_NOREPLACE,
        "publish initialization state",
    )
}

#[cfg(target_os = "linux")]
fn renameat2(from: &Path, to: &Path, flags: u32, action: &str) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", from.display())))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", to.display())))?;
    // SAFETY: both paths are NUL-terminated C strings, and the arguments are
    // the documented `renameat2` syscall ABI. Using the syscall directly
    // avoids a glibc-only `renameat2` symbol, so musl builds retain the same
    // atomic protocol.
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            flags,
        )
    } == 0
    {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    Err(Error::CowUnavailable(format!(
        "failed to {action}: {error}"
    )))
}

#[cfg(target_os = "linux")]
fn remove_regular_directory(path: &Path) -> Result<()> {
    ensure_real_directory(path, "initialization staging directory")?;
    if is_btrfs_subvolume(path)? {
        return Err(Error::CowUnavailable(format!(
            "refusing to remove btrfs staging subvolume as an original workspace: {}",
            path.display()
        )));
    }
    fs::remove_dir_all(path)?;
    sync_parent(path)
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryEntry {
    Missing,
    Directory,
}

#[cfg(target_os = "linux")]
fn directory_entry(path: &Path, description: &str) -> Result<DirectoryEntry> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::CowUnavailable(format!(
                    "refusing unsafe {description}: {}",
                    path.display()
                )));
            }
            Ok(DirectoryEntry::Directory)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DirectoryEntry::Missing),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn ensure_real_directory(path: &Path, description: &str) -> Result<()> {
    match directory_entry(path, description)? {
        DirectoryEntry::Directory => Ok(()),
        DirectoryEntry::Missing => Err(Error::CowUnavailable(format!(
            "{description} is missing: {}",
            path.display()
        ))),
    }
}

#[cfg(target_os = "linux")]
fn invalid_initialization_state(path: &Path, detail: &str) -> Error {
    Error::CowUnavailable(format!(
        "invalid btrfs initialization state for {}: {detail}",
        path.display()
    ))
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
    Ok(fs::metadata(path)?.ino() == BTRFS_SUBVOLUME_INODE)
}

#[cfg(target_os = "linux")]
fn is_btrfs_filesystem(path: &Path) -> Result<bool> {
    Ok(matches!(filesystem(path)?, Filesystem::Btrfs))
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

/// Return whether a source is both clean under `CopyFilter` and safe for a
/// recursive Btrfs snapshot. The single scan avoids walking a dirty artifact
/// tree merely to discover that the established filtered import is required.
#[cfg(target_os = "linux")]
fn source_allows_filtered_snapshot(source: &Path) -> Result<bool> {
    let Some(root) = snapshot_identity(source)? else {
        return Ok(false);
    };
    let filter = CopyFilter::for_source(source);
    let mut directories = vec![source.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(source)
                .map_err(|error| Error::Path(error.to_string()))?;
            if filter.excludes(relative) {
                return Ok(false);
            }
            let file_type = entry.file_type()?;
            // Preserve the filtered-copy contract: retained entries must be
            // representable by every supported strategy, even when Btrfs can
            // snapshot a special entry without materializing it.
            if !file_type.is_file() && !file_type.is_dir() && !file_type.is_symlink() {
                return Err(Error::UnsupportedEntry(path));
            }
            if !file_type.is_dir() {
                continue;
            }
            let Some(identity) = snapshot_identity(&path)? else {
                return Ok(false);
            };
            if identity.mount_id != root.mount_id || identity.inode == BTRFS_SUBVOLUME_INODE {
                return Ok(false);
            }
            directories.push(path);
        }
    }
    Ok(true)
}

/// A snapshot cannot reproduce either a nested Btrfs subvolume or an
/// overlaid mount. `statx` provides the mount ID and inode in one lookup; if
/// the kernel cannot provide both, prefer the established materialized copy
/// path over a potentially incomplete snapshot.
#[cfg(target_os = "linux")]
fn source_has_snapshot_boundary(source: &Path) -> Result<bool> {
    let Some(root) = snapshot_identity(source)? else {
        return Ok(true);
    };
    let mut directories = vec![source.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let Some(identity) = snapshot_identity(&path)? else {
                return Ok(true);
            };
            if identity.mount_id != root.mount_id || identity.inode == BTRFS_SUBVOLUME_INODE {
                return Ok(true);
            }
            directories.push(path);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn prune_filtered_snapshot(snapshot: &Path) -> Result<bool> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let filter = CopyFilter::for_source(snapshot);
    let mut excluded = Vec::new();
    let mut walker = walkdir::WalkDir::new(snapshot)
        .min_depth(1)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(snapshot)
            .expect("walked entries always remain below their root");
        if filter.excludes(relative) {
            excluded.push(entry.path().to_path_buf());
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        // A special entry can appear after the source preflight but before
        // the atomic snapshot. Validate the immutable snapshot as well.
        if !entry.file_type().is_file()
            && !entry.file_type().is_dir()
            && !entry.file_type().is_symlink()
        {
            return Err(Error::UnsupportedEntry(entry.path().to_path_buf()));
        }
        // A nested subvolume created after the source-boundary scan becomes
        // an inode-2 stub in the snapshot. Return before deleting anything so
        // the caller can discard it and materialize from the live source.
        if entry.file_type().is_dir() && fs::metadata(entry.path())?.ino() == BTRFS_EMPTY_STUB_INODE
        {
            return Ok(true);
        }
    }

    let mut modes = BTreeMap::new();
    let removal = (|| {
        for path in excluded {
            make_ancestors_writable(snapshot, &path, &mut modes)?;
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_dir() {
                make_tree_writable(&path, &mut modes)?;
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    })();
    let restore = modes.into_iter().try_for_each(|(path, mode)| {
        if path.exists() {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        Ok::<_, std::io::Error>(())
    });
    removal.and(restore.map_err(Error::from))?;
    Ok(false)
}

#[cfg(target_os = "linux")]
fn make_ancestors_writable(
    snapshot: &Path,
    path: &Path,
    modes: &mut BTreeMap<PathBuf, u32>,
) -> Result<()> {
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
        make_directory_writable(directory, modes)?;
        if directory == snapshot {
            return Ok(());
        }
        ancestor = directory.parent();
    }
    Err(Error::Path(format!(
        "filtered path escaped its snapshot: {}",
        path.display()
    )))
}

#[cfg(target_os = "linux")]
fn make_tree_writable(path: &Path, modes: &mut BTreeMap<PathBuf, u32>) -> Result<()> {
    make_directory_writable(path, modes)?;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_dir() {
            make_directory_writable(entry.path(), modes)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn make_directory_writable(path: &Path, modes: &mut BTreeMap<PathBuf, u32>) -> Result<()> {
    use std::collections::btree_map::Entry;
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)?.permissions().mode();
    if let Entry::Vacant(entry) = modes.entry(path.to_path_buf()) {
        entry.insert(mode);
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o300))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct SnapshotIdentity {
    mount_id: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn snapshot_identity(path: &Path) -> Result<Option<SnapshotIdentity>> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::Path(format!("path contains a null byte: {}", path.display())))?;
    // SAFETY: `stat` is a C struct that the kernel fully initializes on a
    // successful `statx` call.
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    let requested = libc::STATX_INO | libc::STATX_MNT_ID;
    // SAFETY: `path` is a live C string and `stat` points to writable storage
    // with the exact layout required by the documented `statx` syscall ABI.
    // Call the syscall directly rather than glibc's `statx` wrapper so a
    // release binary remains loadable on glibc versions older than 2.28.
    if unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            requested,
            &mut stat,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => Ok(None),
            _ => Err(error.into()),
        };
    }
    if stat.stx_mask & requested != requested {
        return Ok(None);
    }
    Ok(Some(SnapshotIdentity {
        mount_id: stat.stx_mnt_id,
        inode: stat.stx_ino,
    }))
}

#[cfg(target_os = "linux")]
const BTRFS_EMPTY_STUB_INODE: u64 = 2;
#[cfg(target_os = "linux")]
const BTRFS_SUBVOLUME_INODE: u64 = 256;

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
    // SAFETY: `parent` is an open directory fd, and `args` has the C layout
    // expected by the btrfs volume ioctls for the duration of this call.
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

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::{Builder, TempDir};

    fn btrfs_temp() -> Option<TempDir> {
        let temp = Builder::new()
            .prefix(".rift-core-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        is_btrfs_filesystem(temp.path()).unwrap().then_some(temp)
    }

    #[test]
    fn btrfs_integration_environment_is_available() {
        if std::env::var_os("RIFT_REQUIRE_BTRFS_TESTS").is_some() {
            assert!(
                btrfs_temp().is_some(),
                "RIFT_REQUIRE_BTRFS_TESTS requires the checkout filesystem to be btrfs"
            );
        }
    }

    fn set_xattr(path: &Path, name: &str, value: &[u8]) {
        let path = c_path(path).unwrap();
        let name = std::ffi::CString::new(name).unwrap();
        assert_eq!(
            // SAFETY: test inputs are valid C strings and `value` is a live
            // byte slice whose contents are copied by the kernel.
            unsafe {
                libc::lsetxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                )
            },
            0
        );
    }

    fn get_xattr(path: &Path, name: &str) -> Vec<u8> {
        let path = c_path(path).unwrap();
        let name = std::ffi::CString::new(name).unwrap();
        // SAFETY: test inputs are valid C strings. A null buffer with size 0
        // requests the attribute value length.
        let size =
            unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        assert!(size >= 0);
        let mut value = vec![0; size as usize];
        assert_eq!(
            // SAFETY: `value` is allocated with the exact size returned by
            // `lgetxattr`, and the C strings live for this call.
            unsafe {
                libc::lgetxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            },
            size
        );
        value
    }

    fn staged_initialization(source: &Path) -> PendingInitialization {
        let pending = create_pending_initialization(source).unwrap();
        create_btrfs_subvolume(&pending.staging).unwrap();
        import_directory_linux(source, &pending.staging, &mut |_| {}).unwrap();
        copy_metadata_linux(source, &pending.staging, MetadataTarget::FileOrDirectory).unwrap();
        pending
    }

    #[test]
    fn initialization_sidecar_symlink_is_rejected_without_following_it() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let victim = temp.path().join("victim");
        fs::create_dir(&source).unwrap();
        fs::write(&victim, "do not touch").unwrap();
        let state = init_state_path(&source).unwrap();
        std::os::unix::fs::symlink(&victim, &state).unwrap();

        assert!(matches!(
            recover_pending_initialization(&source),
            Err(Error::CowUnavailable(message)) if message.contains("unsafe btrfs initialization sidecar")
        ));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "do not touch");
        assert_eq!(fs::read_link(&state).unwrap(), victim);
    }

    #[test]
    fn native_init_imports_files_links_metadata_and_progress() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        let file = nested.join("file.txt");
        fs::write(&file, "hello").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        set_xattr(&file, "user.rift_test", b"xattr");
        fs::hard_link(&file, nested.join("hard.txt")).unwrap();
        std::os::unix::fs::symlink("file.txt", nested.join("link.txt")).unwrap();
        let mut progress = Vec::new();

        assert_eq!(
            initialize_directory_linux(&source, &mut |event| progress.push(event)).unwrap(),
            StrategyInit::Converted
        );
        assert!(is_btrfs_subvolume(&source).unwrap());
        assert_eq!(
            fs::metadata(&source).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_eq!(
            fs::read_to_string(source.join("nested/file.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            fs::read_link(source.join("nested/link.txt")).unwrap(),
            Path::new("file.txt")
        );
        assert_eq!(
            fs::metadata(source.join("nested/file.txt")).unwrap().ino(),
            fs::metadata(source.join("nested/hard.txt")).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(source.join("nested/file.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            get_xattr(&source.join("nested/file.txt"), "user.rift_test"),
            b"xattr"
        );
        assert!(progress.contains(&InitProgress::CreatingSubvolume));
        assert!(progress.contains(&InitProgress::ImportingWorkspace));
        assert!(progress.contains(&InitProgress::ActivatingWorkspace));
        assert!(progress.contains(&InitProgress::RemovingOriginal));
        assert!(
            progress
                .iter()
                .any(|event| matches!(event, InitProgress::ImportedEntries { .. }))
        );
        remove_directory_linux(&source).unwrap();
    }

    #[test]
    fn native_init_retries_an_interrupted_pre_activation_operation() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "preserve me").unwrap();
        let pending = staged_initialization(&source);
        assert_eq!(
            fs::metadata(&pending.state).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(is_btrfs_subvolume(&pending.staging).unwrap());

        assert_eq!(
            initialize_directory_linux(&source, &mut |_| {}).unwrap(),
            StrategyInit::Converted
        );
        assert!(is_btrfs_subvolume(&source).unwrap());
        assert_eq!(
            fs::read_to_string(source.join("file.txt")).unwrap(),
            "preserve me"
        );
        assert!(!pending.staging.exists());
        assert!(!pending.state.exists());
        remove_directory_linux(&source).unwrap();
    }

    #[test]
    fn native_init_finishes_cleanup_after_an_interrupted_exchange() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "preserve me").unwrap();
        let pending = staged_initialization(&source);

        rename_exchange(&source, &pending.staging).unwrap();
        sync_parent(&source).unwrap();
        assert!(is_btrfs_subvolume(&source).unwrap());
        assert!(!is_btrfs_subvolume(&pending.staging).unwrap());

        assert_eq!(
            initialize_directory_linux(&source, &mut |_| {}).unwrap(),
            StrategyInit::Converted
        );
        assert!(is_btrfs_subvolume(&source).unwrap());
        assert_eq!(
            fs::read_to_string(source.join("file.txt")).unwrap(),
            "preserve me"
        );
        assert!(!pending.staging.exists());
        assert!(!pending.state.exists());
        remove_directory_linux(&source).unwrap();
    }

    #[test]
    fn native_snapshot_and_delete_use_btrfs_strategy() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        let snapshot = temp.path().join("snapshot");
        let filtered = temp.path().join("filtered");
        create_btrfs_subvolume(&source).unwrap();
        fs::write(source.join("file.txt"), "shared before mutation").unwrap();

        copy_directory_linux(&source, &snapshot, CopyMode::All).unwrap();
        assert!(is_btrfs_subvolume(&snapshot).unwrap());
        assert_eq!(
            fs::read_to_string(snapshot.join("file.txt")).unwrap(),
            "shared before mutation"
        );
        assert_copy_diverges_after_mutation(&source.join("file.txt"), &snapshot.join("file.txt"));

        // A clean filtered tree takes the same writable snapshot path as an
        // exact copy. Git refs with artifact-looking names are retained and
        // must not make the preflight report a false exclusion.
        fs::create_dir_all(source.join(".git/refs/heads/build")).unwrap();
        fs::write(source.join(".git/refs/heads/build/main"), "ref").unwrap();
        assert!(source_allows_filtered_snapshot(&source).unwrap());
        copy_directory_linux(&source, &filtered, CopyMode::Filtered).unwrap();
        assert!(is_btrfs_subvolume(&filtered).unwrap());
        assert_eq!(
            fs::read_to_string(filtered.join(".git/refs/heads/build/main")).unwrap(),
            "ref"
        );
        assert_copy_diverges_after_mutation(&source.join("file.txt"), &filtered.join("file.txt"));

        remove_directory_linux(&filtered).unwrap();
        remove_directory_linux(&snapshot).unwrap();
        remove_directory_linux(&source).unwrap();
        assert!(!filtered.exists());
        assert!(!snapshot.exists());
        assert!(!source.exists());
    }

    #[test]
    fn native_filtered_copy_keeps_the_import_path_when_an_artifact_is_present() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        let filtered = temp.path().join("filtered");
        create_btrfs_subvolume(&source).unwrap();
        fs::write(source.join("kept.txt"), "keep").unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "drop").unwrap();

        assert!(!source_allows_filtered_snapshot(&source).unwrap());

        copy_directory_linux(&source, &filtered, CopyMode::Filtered).unwrap();

        assert!(is_btrfs_subvolume(&filtered).unwrap());
        assert_eq!(
            fs::read_to_string(filtered.join("kept.txt")).unwrap(),
            "keep"
        );
        assert!(!filtered.join("node_modules").exists());
        remove_directory_linux(&filtered).unwrap();
        remove_directory_linux(&source).unwrap();
    }

    #[test]
    fn filtering_a_stable_snapshot_prunes_artifacts_from_a_read_only_tree() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        let snapshot = temp.path().join("snapshot");
        create_btrfs_subvolume(&source).unwrap();
        fs::write(source.join("kept.txt"), "keep shared data").unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "drop").unwrap();
        fs::set_permissions(
            source.join("node_modules"),
            fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o500)).unwrap();

        create_btrfs_snapshot(&source, &snapshot).unwrap();
        assert!(!prune_filtered_snapshot(&snapshot).unwrap());

        assert_eq!(
            fs::read_to_string(snapshot.join("kept.txt")).unwrap(),
            "keep shared data"
        );
        assert!(!snapshot.join("node_modules").exists());
        assert!(source.join("node_modules/pkg/index.js").exists());
        assert_eq!(
            fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
            0o500
        );
        // The source intentionally keeps its read-only artifact to prove
        // snapshot pruning did not mutate it. Restore write permission only
        // for the test's non-privileged recursive teardown fallback.
        fs::set_permissions(
            source.join("node_modules"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
        remove_directory_linux(&snapshot).unwrap();
        remove_directory_linux(&source).unwrap();
    }

    #[test]
    fn native_filtered_copy_materializes_nested_subvolume_contents() {
        let Some(temp) = btrfs_temp() else {
            return;
        };
        let source = temp.path().join("source");
        let nested = source.join("nested");
        let filtered = temp.path().join("filtered");
        create_btrfs_subvolume(&source).unwrap();
        create_btrfs_subvolume(&nested).unwrap();
        fs::write(nested.join("kept.txt"), "keep nested content").unwrap();

        assert!(!source_allows_filtered_snapshot(&source).unwrap());
        assert!(source_has_snapshot_boundary(&source).unwrap());
        copy_directory_linux(&source, &filtered, CopyMode::Filtered).unwrap();

        assert_eq!(
            fs::read_to_string(filtered.join("nested/kept.txt")).unwrap(),
            "keep nested content"
        );
        remove_directory_linux(&filtered).unwrap();
        remove_directory_linux(&nested).unwrap();
        remove_directory_linux(&source).unwrap();
    }

    #[test]
    fn native_strategy_reports_non_btrfs_and_unsupported_entries() {
        let temp = TempDir::new().unwrap();
        assert!(!is_btrfs_filesystem(temp.path()).unwrap());
        assert!(!is_btrfs_subvolume(temp.path()).unwrap());
        assert!(matches!(
            initialize_directory_linux(temp.path(), &mut |_| {}),
            Err(Error::CowUnavailable(_))
        ));
        assert!(matches!(
            copy_directory_linux(temp.path(), &temp.path().join("snapshot"), CopyMode::All),
            Err(Error::CowUnavailable(_))
        ));

        let Some(btrfs) = btrfs_temp() else {
            return;
        };
        let from = btrfs.path().join("source");
        let to = btrfs.path().join("destination");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        let fifo = from.join("fifo");
        let fifo_name = c_path(&fifo).unwrap();
        // SAFETY: `fifo_name` is a valid C path and the mode is a normal
        // permission bitmask for creating a FIFO in this test.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            import_directory_linux(&from, &to, &mut |_| {}),
            Err(Error::UnsupportedEntry(path)) if path == fifo
        ));
    }

    #[test]
    fn native_fallback_removes_populated_tree() {
        let temp = TempDir::new().unwrap();
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(tree.join("nested")).unwrap();
        fs::write(tree.join("nested/file.txt"), "hello").unwrap();
        remove_emptyable_subvolume(&tree).unwrap();
        assert!(!tree.exists());
    }

    fn assert_copy_diverges_after_mutation(source: &Path, clone: &Path) {
        let original = fs::read_to_string(source).unwrap();
        assert_eq!(fs::read_to_string(clone).unwrap(), original);
        fs::write(source, "parent mutation").unwrap();
        assert_eq!(fs::read_to_string(clone).unwrap(), original);
        fs::write(clone, "child mutation").unwrap();
        assert_eq!(fs::read_to_string(source).unwrap(), "parent mutation");
        assert_eq!(fs::read_to_string(clone).unwrap(), "child mutation");
    }
}
