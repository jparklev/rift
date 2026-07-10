mod config;
mod filter;
mod git;
mod hook;
mod id;
mod lock;
mod marker;
mod name;
mod registry;
mod strategy;

#[cfg(all(test, target_os = "linux"))]
mod linux_filesystem_tests;
#[cfg(all(test, target_os = "linux"))]
mod test_support;

use id::RiftId;
use lock::LockDirectory;
use name::RiftName;
use registry::{MovedRecord, PathRecord, PendingRemoval, Record, Registry, SubtreeScope};
use std::fs;
use std::path::{Path, PathBuf};
use strategy::{Strategy, StrategyInit};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Database(#[from] rusqlite::Error),
    #[error("{0}")]
    Walk(#[from] walkdir::Error),
    #[error("invalid path: {0}")]
    Path(String),
    #[error("copy-on-write cloning unavailable: {0}")]
    CowUnavailable(String),
    #[error("workspace requires initialization: {0}")]
    InitializationRequired(PathBuf),
    #[error("workspace is not initialized: {0}")]
    WorkspaceNotInitialized(PathBuf),
    #[error("rift marker is missing: {0}")]
    MissingMarker(PathBuf),
    #[error("unsupported filesystem entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("unsafe Git source: {0}")]
    UnsafeGit(String),
    #[error("directory is not managed by rift: {0}")]
    NotManaged(PathBuf),
    #[error("rift marker does not match the registry at: {0}")]
    MarkerMismatch(PathBuf),
    #[error("rift marker must be a regular file: {0}")]
    UnsafeMarker(PathBuf),
    #[error("rift marker belongs to an unknown registry entry at: {0}")]
    UnknownMarker(PathBuf),
    #[error("rift directory already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("cannot remove subtree while a recorded rift path is missing: {0}")]
    MissingRift(PathBuf),
    #[error(
        "workspace {workspace} references parent record {parent_id}, which is missing from the registry"
    )]
    DanglingParent {
        workspace: PathBuf,
        parent_id: String,
    },
    #[error("cannot copy a workspace into itself: {0}")]
    InsideSource(PathBuf),
    #[error("invalid rift config at {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
    #[error("postcreate hook failed at {path}: `{command}` {message}")]
    HookFailed {
        path: PathBuf,
        command: String,
        message: String,
    },
}

pub struct Create {
    pub from: PathBuf,
    pub name: Option<String>,
    pub into: Option<PathBuf>,
}

/// A managed workspace resolved from a path: its root, identifier, and the
/// immediate parent workspace it was created from (none for a source root).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub path: PathBuf,
    pub id: String,
    pub parent: Option<PathBuf>,
}

impl Create {
    pub fn new(from: impl Into<PathBuf>) -> Self {
        Self {
            from: from.into(),
            name: None,
            into: None,
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    pub fn with_storage(mut self, into: Option<PathBuf>) -> Self {
        self.into = into;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CreateOptions {
    pub copy_mode: CopyMode,
    pub hook_mode: HookMode,
}

impl CreateOptions {
    pub fn copy_mode(mut self, copy_mode: CopyMode) -> Self {
        self.copy_mode = copy_mode;
        self
    }

    pub fn hook_mode(mut self, hook_mode: HookMode) -> Self {
        self.hook_mode = hook_mode;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CopyMode {
    #[default]
    Filtered,
    All,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HookMode {
    #[default]
    Run,
    Skip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitProgress {
    CreatingSubvolume,
    ImportingWorkspace,
    ImportedEntries { entries: u64 },
    ActivatingWorkspace,
    RemovingOriginal,
    RestoringMarker,
    RegisteringWorkspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitOutcome {
    Registered,
    AlreadyInitialized,
    Converted,
}

impl InitOutcome {
    pub fn is_converted(self) -> bool {
        matches!(self, Self::Converted)
    }
}

pub struct Manager {
    registry: Registry,
    strategy: Box<dyn Strategy>,
    locks: LockDirectory,
}

impl Manager {
    pub fn open_default() -> Result<Self> {
        let path = default_database_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::open(path)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_strategy(path, strategy::default_strategy())
    }

    fn with_strategy(path: impl AsRef<Path>, strategy: Box<dyn Strategy>) -> Result<Self> {
        let path = path.as_ref();
        let locks = LockDirectory::open(path.with_extension("locks"))?;
        let registry = Registry::open(path)?;
        let mut manager = Self {
            registry,
            strategy,
            locks,
        };
        // Finish unambiguous interrupted removals eagerly, but never make an
        // unrelated workspace unusable because one root needs manual repair.
        // Mutations on that root re-run recovery while holding its own lock
        // and surface the conflict there.
        manager.recover_pending_removals()?;
        Ok(manager)
    }

    pub fn create(&mut self, input: Create) -> Result<PathBuf> {
        self.create_with_options(input, CreateOptions::default())
    }

    pub fn create_with_options(
        &mut self,
        input: Create,
        options: CreateOptions,
    ) -> Result<PathBuf> {
        let requested = existing_directory(&input.from)?;
        let initial_source = self.workspace_from(&requested)?;
        let initial_root = self.root(&initial_source)?;
        let _lock = self.locks.lock_root(&initial_root.id)?;
        self.recover_pending_removals_for_locked_root(&initial_root.id)?;
        // A root unregister may have completed between the first lookup and
        // lock acquisition. Re-resolve under the lock so a create never adds a
        // child below a root that has already been deleted.
        let source = self.workspace_from(&requested)?;
        let root = self.root(&source)?;
        if root.id != initial_root.id {
            return Err(Error::NotManaged(requested));
        }
        let from = source.path.clone();
        let git = git::check_source(&from)?;
        let id = RiftId::new();
        let requested_name = input
            .name
            .map(|name| RiftName::from_optional(Some(name)))
            .transpose()?;
        let destination_parent = match input.into {
            Some(path) => absolute_path(&path)?,
            None => default_storage(&root.path)?,
        };
        if destination_parent.starts_with(&from) {
            return Err(Error::InsideSource(destination_parent));
        }
        fs::create_dir_all(&destination_parent)?;
        let destination_parent = fs::canonicalize(destination_parent)?;
        let destination = match requested_name {
            Some(name) => {
                let destination = destination_parent.join(name.as_str());
                if path_entry_exists(&destination)? {
                    return Err(Error::AlreadyExists(destination));
                }
                destination
            }
            None => generated_destination(&destination_parent, &id)?,
        };
        if destination.starts_with(&from) {
            return Err(Error::InsideSource(destination));
        }
        let config = match options.hook_mode {
            HookMode::Run => config::Config::load(&from)?,
            HookMode::Skip => config::Config::default(),
        };

        if let Err(error) = self
            .strategy
            .copy_directory(&from, &destination, options.copy_mode)
        {
            if destination.exists() {
                let _ = self.strategy.remove_directory(&destination);
            }
            return Err(error);
        }

        let result: Result<()> = (|| {
            marker::write(&destination, &id)?;
            if git.is_repository() {
                git::hide_marker(&destination)?;
                git::detach_destination(&destination)?;
            }
            if git.is_repository() {
                git::hide_marker(&from)?;
            }
            self.registry.insert_child(&id, &source.id, &destination)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.strategy.remove_directory(&destination);
        }
        result?;
        hook::run_postcreate(config.postcreate(), &from, &destination, &id, &source.id)?;
        Ok(destination)
    }

    pub fn init(&mut self, at: impl AsRef<Path>) -> Result<InitOutcome> {
        self.init_with_progress(at, |_| {})
    }

    pub fn init_with_progress(
        &mut self,
        at: impl AsRef<Path>,
        mut progress: impl FnMut(InitProgress),
    ) -> Result<InitOutcome> {
        let at = existing_directory(at.as_ref())?;
        let _initialization_lock = self.locks.lock_initialization(&at)?;
        let git = git::check_source(&at)?;
        if let Some(initial_record) = self.registry.record_at(&at)? {
            let initial_root = self.root(&initial_record)?;
            let _lock = self.locks.lock_root(&initial_root.id)?;
            self.recover_pending_removals_for_locked_root(&initial_root.id)?;
            // Re-resolve after the lock and recovery: a concurrent root
            // unregister may have completed while this call waited.
            let record = self
                .registry
                .record_at(&at)?
                .ok_or_else(|| Error::WorkspaceNotInitialized(at.clone()))?;
            if self.root(&record)?.id != initial_root.id {
                return Err(Error::NotManaged(at));
            }
            if marker::read(&at)?.is_none() {
                progress(InitProgress::RestoringMarker);
                marker::write(&at, &record.id)?;
            } else {
                marker::verify(&record.path, &record.id)?;
            }
            let converted = self.strategy.initialize_directory(&at, &mut progress)?;
            if git.is_repository() {
                git::hide_marker(&at)?;
            }
            return Ok(match converted {
                StrategyInit::AlreadyNative => InitOutcome::AlreadyInitialized,
                StrategyInit::Converted => InitOutcome::Converted,
            });
        }
        if marker::read(&at)?.is_some() {
            return Err(Error::MarkerMismatch(at));
        }

        let converted = self.strategy.initialize_directory(&at, &mut progress)?;
        progress(InitProgress::RegisteringWorkspace);
        let id = RiftId::new();
        let result = (|| {
            marker::write(&at, &id)?;
            if git.is_repository() {
                git::hide_marker(&at)?;
            }
            self.registry.insert_root(&id, &at)?;
            Ok(match converted {
                StrategyInit::AlreadyNative => InitOutcome::Registered,
                StrategyInit::Converted => InitOutcome::Converted,
            })
        })();
        if result.is_err() {
            // Do not unlink another process's marker if an external actor
            // replaced ours after the failed registration attempt.
            if matches!(marker::read(&at), Ok(Some(written)) if written == id) {
                let _ = marker::remove_regular(&at);
            }
        }
        result
    }

    pub fn remove(&mut self, at: impl AsRef<Path>) -> Result<()> {
        let (record, root, _lock) = self.locked_workspace(at)?;
        if record.parent_id.is_none() {
            return self.unregister_root(&record);
        }
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::IncludingRoot)?;
        self.trash_rows(&root, &rows, false)
    }

    pub fn remove_all(&mut self, at: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let (record, root, _lock) = self.locked_workspace(at)?;
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?;
        self.trash_rows(&root, &rows, false)?;
        Ok(rows.into_iter().map(|record| record.path).collect())
    }

    fn unregister_root(&mut self, record: &Record) -> Result<()> {
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?;
        let existing = rows.into_iter().try_fold(
            Vec::new(),
            |mut existing, record| -> Result<Vec<PathRecord>> {
                if path_entry_exists(&record.path)? {
                    existing.push(record);
                }
                Ok(existing)
            },
        )?;
        self.trash_rows(record, &existing, true)
    }

    fn trash_rows(
        &mut self,
        root: &Record,
        rows: &[PathRecord],
        unregister_root: bool,
    ) -> Result<()> {
        rows.iter().try_for_each(|row| -> Result<()> {
            path_entry_exists(&row.path)?
                .then_some(())
                .ok_or_else(|| Error::MissingRift(row.path.clone()))?;
            marker::verify(&row.path, &row.id)?;
            Ok(())
        })?;
        let targets = rows
            .iter()
            .map(|row| {
                Ok(MovedRecord {
                    id: row.id.clone(),
                    original_path: row.path.clone(),
                    trash_path: trash_path(&row.id, &row.path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        targets.iter().try_for_each(|target| {
            (!path_entry_exists(&target.trash_path)?)
                .then_some(())
                .ok_or_else(|| Error::AlreadyExists(target.trash_path.clone()))
        })?;
        if targets.is_empty() && !unregister_root {
            return Ok(());
        }
        let operation = self
            .registry
            .stage_removal(root, unregister_root, &targets)?;
        self.finish_pending_removal(&operation)
    }

    /// Complete a durable removal operation. It is intentionally idempotent:
    /// after a process exit, an original path may already be absent while its
    /// trash path is present. Ambiguous states stay journaled rather than
    /// guessing which directory is safe to remove.
    fn finish_pending_removal(&mut self, operation: &PendingRemoval) -> Result<()> {
        for target in &operation.moved {
            match (
                path_entry_exists(&target.original_path)?,
                path_entry_exists(&target.trash_path)?,
            ) {
                (true, false) => {
                    let trash_parent = target.trash_path.parent().ok_or_else(|| {
                        Error::Path(format!(
                            "trash path has no parent: {}",
                            target.trash_path.display()
                        ))
                    })?;
                    fs::create_dir_all(trash_parent)?;
                    fs::rename(&target.original_path, &target.trash_path)?;
                }
                (false, true) => {}
                (true, true) => return Err(Error::AlreadyExists(target.trash_path.clone())),
                (false, false) => return Err(Error::MissingRift(target.original_path.clone())),
            }
        }
        if operation.unregister_root {
            // This is idempotent for an absent marker, so recovery also works
            // when a process exited after unlinking it but before SQL commit.
            marker::remove_regular(&operation.root_path)?;
        }
        self.registry.complete_removal(operation)
    }

    fn recover_pending_removals(&mut self) -> Result<()> {
        for pending in self.registry.pending_removals()? {
            let _lock = self.locks.lock_root(&pending.root_id)?;
            // A conflicting operation is intentionally left journaled. It
            // must block mutations on its own root, not opening or collecting
            // unrelated roots from the same registry.
            let _ = self.recover_pending_removals_for_locked_root(&pending.root_id);
        }
        Ok(())
    }

    fn recover_pending_removals_for_locked_root(&mut self, root_id: &RiftId) -> Result<()> {
        for pending in self.registry.pending_removals_for_root(root_id)? {
            // Another process may have finished the operation while this
            // manager waited for the same root lock.
            if let Some(pending) = self.registry.pending_removal(&pending.id)? {
                self.finish_pending_removal(&pending)?;
            }
        }
        Ok(())
    }

    /// Resolve once to find the lock, acquire it, then resolve again. This
    /// closes the create/root-remove race without serializing unrelated roots.
    fn locked_workspace(
        &mut self,
        at: impl AsRef<Path>,
    ) -> Result<(Record, Record, lock::LifecycleLock)> {
        let requested = existing_directory(at.as_ref())?;
        let initial = self.workspace_from(&requested)?;
        let initial_root = self.root(&initial)?;
        let lock = self.locks.lock_root(&initial_root.id)?;
        self.recover_pending_removals_for_locked_root(&initial_root.id)?;
        let record = self.workspace_from(&requested)?;
        let root = self.root(&record)?;
        if root.id != initial_root.id {
            return Err(Error::NotManaged(requested));
        }
        Ok((record, root, lock))
    }

    pub fn list(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        self.registry.child_paths(&record.id)
    }

    pub fn ancestors(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        let mut paths = Vec::new();
        let mut parent_id = record.parent_id;
        while let Some(id) = parent_id {
            let parent = self
                .registry
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
            paths.push(parent.path);
            parent_id = parent.parent_id;
        }
        Ok(paths)
    }

    pub fn gc(&mut self) -> Result<Vec<PathBuf>> {
        self.reclaim_trash()
    }

    /// Reclaim only workspaces that Rift has already moved into its owned
    /// trash. This is intentionally narrower than administrative registry
    /// repair and is safe for the CLI's automatic post-remove cleanup.
    pub fn reclaim_trash(&mut self) -> Result<Vec<PathBuf>> {
        // Garbage collection only reclaims directories that were already
        // logically moved to Rift-owned trash. It must never infer that an
        // active workspace was deleted merely because a volume is temporarily
        // unmounted or a directory is being renamed outside Rift.
        let _lock = self.locks.lock_gc()?;
        self.registry
            .trashed_paths()?
            .into_iter()
            .map(|row| -> Result<PathBuf> {
                if row.path.exists() {
                    self.strategy.remove_directory(&row.path)?;
                }
                self.registry.delete_trash(&row.id)?;
                // Prune the `.trash` container once its last entry is gone;
                // `remove_dir` refuses non-empty directories, so a racing
                // trash insert keeps it in place.
                if let Some(parent) = row
                    .path
                    .parent()
                    .filter(|parent| parent.file_name() == Some(std::ffi::OsStr::new(".trash")))
                {
                    let _ = fs::remove_dir(parent);
                }
                Ok(row.path)
            })
            .collect()
    }

    pub fn workspace(&self, at: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self.workspace_at(at)?.path)
    }

    /// Resolve the managed workspace governing `at`, reporting its root path,
    /// identifier, and immediate parent workspace (none for a source root).
    pub fn describe(&self, at: impl AsRef<Path>) -> Result<Workspace> {
        let record = self.workspace_at(at)?;
        let parent = match &record.parent_id {
            Some(id) => Some(
                self.registry
                    .record_id(id)?
                    .ok_or_else(|| Error::DanglingParent {
                        workspace: record.path.clone(),
                        parent_id: id.to_string(),
                    })?
                    .path,
            ),
            None => None,
        };
        Ok(Workspace {
            id: record.id.as_str().to_owned(),
            path: record.path,
            parent,
        })
    }

    fn workspace_at(&self, path: impl AsRef<Path>) -> Result<Record> {
        let path = existing_directory(path.as_ref())?;
        self.workspace_from(&path)
    }

    fn workspace_from(&self, path: &Path) -> Result<Record> {
        self.workspace_from_optional(path)?
            .ok_or_else(|| Error::WorkspaceNotInitialized(path.to_path_buf()))
    }

    fn workspace_from_optional(&self, path: &Path) -> Result<Option<Record>> {
        for directory in path.ancestors() {
            if let Some(id) = marker::read(directory)? {
                let record = self
                    .registry
                    .record_id(&id)?
                    .ok_or_else(|| Error::UnknownMarker(directory.to_path_buf()))?;
                if record.path != directory {
                    return Err(Error::MarkerMismatch(directory.to_path_buf()));
                }
                return Ok(Some(record));
            }
            if self.registry.record_at(directory)?.is_some() {
                return Err(Error::MissingMarker(directory.to_path_buf()));
            }
        }
        Ok(None)
    }

    fn root(&self, record: &Record) -> Result<Record> {
        let mut current = record.clone();
        while let Some(id) = current.parent_id.clone() {
            current = self
                .registry
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
        }
        Ok(current)
    }
}

fn default_database_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .ok_or_else(|| Error::Path("user data directory is unavailable".into()))?;
    Ok(base.join("rift").join("rift.sqlite"))
}

fn existing_directory(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(Error::Path(format!("not a directory: {}", path.display())));
    }
    Ok(path)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Unlike `Path::exists`, this reports a dangling symlink as an existing
/// filesystem entry. Lifecycle targets must never treat one as free.
fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

const READABLE_NAME_ATTEMPTS: usize = 32;

/// Select an unused automatic workspace destination while the caller holds
/// the provenance-root lock. The readable two-word space is deliberately
/// small for ergonomics, so a bounded retry followed by a Rift-ID suffix keeps
/// automatic creation reliable even in long-lived storage directories.
fn generated_destination(parent: &Path, id: &RiftId) -> Result<PathBuf> {
    generated_destination_with(parent, id, RiftName::generated)
}

fn generated_destination_with(
    parent: &Path,
    id: &RiftId,
    mut next_name: impl FnMut() -> RiftName,
) -> Result<PathBuf> {
    for _ in 0..READABLE_NAME_ATTEMPTS {
        let destination = parent.join(next_name().as_str());
        if !path_entry_exists(&destination)? {
            return Ok(destination);
        }
    }

    for attempt in 0..READABLE_NAME_ATTEMPTS {
        let suffix = format!("{}-{attempt}", id.as_str());
        let destination = parent.join(RiftName::generated_with_suffix(&suffix).as_str());
        if !path_entry_exists(&destination)? {
            return Ok(destination);
        }
    }

    Err(Error::AlreadyExists(parent.to_path_buf()))
}

fn default_storage(root: &Path) -> Result<PathBuf> {
    let parent = root
        .parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", root.display())))?;
    let name = root
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", root.display())))?;
    Ok(parent.join(".rifts").join(name))
}

fn trash_path(id: &RiftId, path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Path(format!("rift has no parent: {}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("rift has no name: {}", path.display())))?;
    Ok(parent
        .join(".trash")
        .join(format!("{id}-{}", name.to_string_lossy())))
}

#[cfg(test)]
mod tests;
