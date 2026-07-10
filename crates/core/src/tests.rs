use super::*;
use crate::strategy::{FailureStrategy, Strategy, TestStrategy};
use std::cell::Cell;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;
use ulid::Ulid;

fn manager(temp: &TempDir) -> Manager {
    Manager::with_strategy(temp.path().join("registry.sqlite"), Box::new(TestStrategy)).unwrap()
}

fn source(temp: &TempDir) -> PathBuf {
    let source = temp.path().join("app");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("file.txt"), "hello").unwrap();
    fs::canonicalize(source).unwrap()
}

fn marker_id(path: &Path) -> RiftId {
    marker::read(path).unwrap().unwrap()
}

fn create_input(from: PathBuf, name: &str) -> Create {
    Create::new(from).named(name)
}

fn create_options(copy_mode: CopyMode, hook_mode: HookMode) -> CreateOptions {
    CreateOptions::default()
        .copy_mode(copy_mode)
        .hook_mode(hook_mode)
}

fn child_path(source: &Path, name: &str) -> PathBuf {
    source.parent().unwrap().join(".rifts/app").join(name)
}

struct BlockingCopyStrategy {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl Strategy for BlockingCopyStrategy {
    fn copy_directory(&self, from: &Path, to: &Path, mode: CopyMode) -> Result<()> {
        // `Manager::create` reaches this only after acquiring the root lock.
        self.entered.wait();
        self.release.wait();
        TestStrategy.copy_directory(from, to, mode)
    }
}

struct BlockingInitStrategy {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl Strategy for BlockingInitStrategy {
    fn copy_directory(&self, _from: &Path, _to: &Path, _mode: CopyMode) -> Result<()> {
        unreachable!()
    }

    fn initialize_directory(
        &self,
        _path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
    ) -> Result<StrategyInit> {
        // `Manager::init` reaches this only after acquiring the path-derived
        // first-initialization lock.
        self.entered.wait();
        self.release.wait();
        Ok(StrategyInit::AlreadyNative)
    }
}

#[test]
fn create_tracks_parentage_and_default_storage() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();

    let parent = source.parent().unwrap();
    assert_eq!(first, parent.join(".rifts/app/first"));
    assert_eq!(second, parent.join(".rifts/app/second"));
    assert_ne!(
        fs::read_to_string(source.join(".rift")).unwrap(),
        fs::read_to_string(first.join(".rift")).unwrap()
    );
    assert_eq!(manager.list(&source).unwrap(), vec![first.clone()]);
    assert_eq!(manager.ancestors(&second).unwrap(), vec![first, source]);
}

#[test]
fn root_remove_waits_for_an_inflight_create_before_unregistering() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let database = temp.path().join("registry.sqlite");
    let mut setup = manager(&temp);
    setup.init(&source).unwrap();
    drop(setup);

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let create_source = source.clone();
    let create_database = database.clone();
    let create_entered = entered.clone();
    let create_release = release.clone();
    let create = thread::spawn(move || {
        let mut manager = Manager::with_strategy(
            create_database,
            Box::new(BlockingCopyStrategy {
                entered: create_entered,
                release: create_release,
            }),
        )
        .unwrap();
        manager.create(Create::new(create_source).named("racing-child"))
    });

    // The create owns the root lock and is paused in its copy strategy.
    entered.wait();
    let (removed_tx, removed_rx) = mpsc::channel();
    let remove_source = source.clone();
    let remove_database = database;
    let remove = thread::spawn(move || {
        let mut manager = Manager::with_strategy(remove_database, Box::new(TestStrategy)).unwrap();
        let result = manager.remove(remove_source);
        removed_tx.send(result).unwrap();
    });

    let early_remove = removed_rx.recv_timeout(Duration::from_millis(100));
    let removal_bypassed_lock = early_remove.is_ok();
    // Always unblock the creator before asserting so a failed regression never
    // strands a test thread at the barrier.
    release.wait();
    let child = create.join().unwrap().unwrap();
    let removed = match early_remove {
        Ok(result) => result,
        Err(_) => removed_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
    };
    remove.join().unwrap();

    assert!(
        !removal_bypassed_lock,
        "root removal bypassed the create lock"
    );
    removed.unwrap();
    assert!(!child.exists());
    let reopened = manager(&temp);
    assert!(matches!(
        reopened.list(&source),
        Err(Error::WorkspaceNotInitialized(_))
    ));
}

#[test]
fn concurrent_first_initialization_keeps_the_winning_marker() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let database = temp.path().join("registry.sqlite");
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let first_source = source.clone();
    let first_database = database.clone();
    let first_entered = entered.clone();
    let first_release = release.clone();
    let first = thread::spawn(move || {
        let mut manager = Manager::with_strategy(
            first_database,
            Box::new(BlockingInitStrategy {
                entered: first_entered,
                release: first_release,
            }),
        )
        .unwrap();
        manager.init(first_source)
    });

    // The first init holds the canonical-path lock while its strategy pauses.
    entered.wait();
    let (second_tx, second_rx) = mpsc::channel();
    let second_source = source.clone();
    let second = thread::spawn(move || {
        let mut manager = Manager::with_strategy(database, Box::new(TestStrategy)).unwrap();
        second_tx.send(manager.init(second_source)).unwrap();
    });
    let early_second = second_rx.recv_timeout(Duration::from_millis(100));
    let second_bypassed_lock = early_second.is_ok();

    // Always release the first initializer before asserting so a regression
    // cannot leave a test thread trapped at the barrier.
    release.wait();
    assert_eq!(first.join().unwrap().unwrap(), InitOutcome::Registered);
    let second_outcome = match early_second {
        Ok(outcome) => outcome,
        Err(_) => second_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
    };
    second.join().unwrap();

    assert!(
        !second_bypassed_lock,
        "second initializer bypassed the path lock"
    );
    assert_eq!(second_outcome.unwrap(), InitOutcome::AlreadyInitialized);
    let verified = manager(&temp);
    assert!(source.join(".rift").is_file());
    assert_eq!(verified.describe(&source).unwrap().parent, None);
}

#[test]
fn init_registers_a_root_workspace_without_creating_a_child() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);

    assert_eq!(manager.init(&source).unwrap(), InitOutcome::Registered);
    assert!(source.join(".rift").exists());
    assert!(manager.list(&source).unwrap().is_empty());
    assert_eq!(
        manager.init(&source).unwrap(),
        InitOutcome::AlreadyInitialized
    );
}

#[test]
fn init_reports_structured_registration_progress_when_requested() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    let mut progress = Vec::new();

    manager
        .init_with_progress(&source, |event| progress.push(event))
        .unwrap();

    assert_eq!(progress, vec![InitProgress::RegisteringWorkspace]);
}

#[test]
fn create_supports_custom_storage_and_rejects_invalid_destinations() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let custom = temp.path().join("custom");
    let child = manager
        .create(
            Create::new(source.clone())
                .named("custom")
                .with_storage(Some(custom.clone())),
        )
        .unwrap();
    assert_eq!(child, fs::canonicalize(&custom).unwrap().join("custom"));
    assert!(matches!(
        manager.create(
            Create::new(source.clone())
                .named("custom")
                .with_storage(Some(custom))
        ),
        Err(Error::AlreadyExists(_))
    ));
    assert!(matches!(
        manager.create(Create::new(source.clone()).named("..")),
        Err(Error::Path(_))
    ));
    assert!(matches!(
        manager.create(Create::new(source.clone()).named(".trash")),
        Err(Error::Path(_))
    ));
    assert!(matches!(
        manager.create(
            Create::new(source.clone())
                .named("inside")
                .with_storage(Some(source.join("nested")))
        ),
        Err(Error::InsideSource(_))
    ));
    assert!(matches!(
        manager.create(Create::new(source.join("file.txt")).named("file")),
        Err(Error::Path(_))
    ));
}

#[test]
fn create_filters_regenerable_artifacts_by_default() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();
    fs::create_dir_all(source.join("target/debug")).unwrap();
    fs::write(source.join("target/debug/app"), "binary").unwrap();
    fs::create_dir_all(source.join(".yarn/cache")).unwrap();
    fs::write(source.join(".yarn/cache/pkg.zip"), "cache").unwrap();
    fs::write(source.join("package.json"), "{}").unwrap();
    fs::write(source.join("Cargo.lock"), "lock").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let child = manager
        .create(create_input(source.clone(), "filtered"))
        .unwrap();

    assert!(!child.join("node_modules").exists());
    assert!(!child.join("target").exists());
    assert!(!child.join(".yarn/cache").exists());
    assert_eq!(
        fs::read_to_string(child.join("package.json")).unwrap(),
        "{}"
    );
    assert_eq!(
        fs::read_to_string(child.join("Cargo.lock")).unwrap(),
        "lock"
    );
}

#[test]
fn create_copy_all_preserves_regenerable_artifacts() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "module").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let child = manager
        .create_with_options(
            create_input(source.clone(), "copy-all"),
            create_options(CopyMode::All, HookMode::Run),
        )
        .unwrap();

    assert_eq!(
        fs::read_to_string(child.join("node_modules/pkg/index.js")).unwrap(),
        "module"
    );
}

#[test]
fn create_runs_postcreate_hooks_in_destination_order() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(
        source.join(".rift.toml"),
        r#"
version = 1

[[hooks.postcreate]]
run = "echo first >> hook.log"

[[hooks.postcreate]]
run = "echo second >> hook.log"
"#,
    )
    .unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let child = manager.create(create_input(source, "hooks")).unwrap();
    let log = fs::read_to_string(child.join("hook.log")).unwrap();

    assert!(log.find("first").unwrap() < log.find("second").unwrap());
}

#[test]
fn postcreate_failure_leaves_registered_workspace() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(
        source.join(".rift.toml"),
        r#"
version = 1

[[hooks.postcreate]]
run = "echo before >> hook.log"

[[hooks.postcreate]]
run = "exit 7"

[[hooks.postcreate]]
run = "echo after >> hook.log"
"#,
    )
    .unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let expected = child_path(&source, "hook-failure");

    let error = manager
        .create(create_input(source.clone(), "hook-failure"))
        .unwrap_err();

    assert!(matches!(
        error,
        Error::HookFailed { path, .. } if path == expected
    ));
    assert!(expected.exists());
    assert_eq!(manager.list(&source).unwrap(), vec![expected.clone()]);
    let log = fs::read_to_string(expected.join("hook.log")).unwrap();
    assert!(log.contains("before"));
    assert!(!log.contains("after"));
}

#[test]
fn hook_skip_ignores_invalid_config() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(source.join(".rift.toml"), "version = 2\n").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let child = manager
        .create_with_options(
            create_input(source, "skip-hooks"),
            create_options(CopyMode::Filtered, HookMode::Skip),
        )
        .unwrap();

    assert!(child.join(".rift.toml").exists());
}

#[test]
fn invalid_hook_config_fails_before_copying() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    fs::write(source.join(".rift.toml"), "version = 2\n").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let expected = child_path(&source, "invalid-config");

    let error = manager
        .create(create_input(source, "invalid-config"))
        .unwrap_err();

    assert!(matches!(error, Error::InvalidConfig { .. }));
    assert!(!expected.exists());
}

#[test]
fn corrupt_and_unknown_markers_are_rejected() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let nested = source.join("nested");
    fs::create_dir(&nested).unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    fs::write(source.join(".rift"), "unknown\n").unwrap();
    assert!(matches!(
        manager.list(&nested),
        Err(Error::UnknownMarker(_))
    ));

    let id = manager.registry.record_at(&source).unwrap().unwrap().id;
    fs::write(source.join(".rift"), format!("{id}\n")).unwrap();
    let other = temp.path().join("other");
    fs::create_dir(&other).unwrap();
    fs::write(other.join(".rift"), format!("{id}\n")).unwrap();
    assert!(matches!(
        manager.list(&other),
        Err(Error::MarkerMismatch(_))
    ));
}

#[test]
fn removal_rejects_marker_mismatch_and_existing_trash_target() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager
        .create(Create::new(source.clone()).named("child"))
        .unwrap();
    let id = marker_id(&child);
    fs::write(child.join(".rift"), "wrong\n").unwrap();
    assert!(matches!(
        manager.remove(&child),
        Err(Error::UnknownMarker(_))
    ));
    fs::write(
        child.join(".rift"),
        fs::read_to_string(source.join(".rift")).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        manager.remove(&child),
        Err(Error::MarkerMismatch(_))
    ));
    fs::write(child.join(".rift"), format!("{id}\n")).unwrap();
    let trash = trash_path(&id, &child).unwrap();
    fs::create_dir_all(&trash).unwrap();
    assert!(matches!(
        manager.remove(&child),
        Err(Error::AlreadyExists(_))
    ));
}

#[test]
fn gc_forgets_a_trashed_path_already_removed_on_disk() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager.create(Create::new(source).named("child")).unwrap();
    let id = marker_id(&child);
    let trash = trash_path(&id, &child).unwrap();
    manager.remove(&child).unwrap();
    fs::remove_dir_all(&trash).unwrap();

    assert_eq!(manager.gc().unwrap(), vec![trash]);
}

#[test]
fn reopening_recovers_a_move_staged_before_registry_transfer() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut initial = manager(&temp);
    initial.init(&source).unwrap();
    let child = initial
        .create(Create::new(source.clone()).named("child"))
        .unwrap();
    let child_id = marker_id(&child);
    let trash = trash_path(&child_id, &child).unwrap();
    let root = initial
        .root(&initial.workspace_at(&source).unwrap())
        .unwrap();
    let operation = initial
        .registry
        .stage_removal(
            &root,
            false,
            &[MovedRecord {
                id: child_id,
                original_path: child.clone(),
                trash_path: trash.clone(),
            }],
        )
        .unwrap();

    // Model the exact crash window: the durable intent and filesystem rename
    // exist, but the active-to-trash registry transaction has not run.
    fs::create_dir_all(trash.parent().unwrap()).unwrap();
    fs::rename(&child, &trash).unwrap();
    drop(initial);

    let mut recovered = manager(&temp);
    assert!(!child.exists());
    assert!(trash.exists());
    assert!(
        recovered
            .registry
            .pending_removal(&operation.id)
            .unwrap()
            .is_none()
    );
    assert!(recovered.list(&source).unwrap().is_empty());
    assert_eq!(recovered.gc().unwrap(), vec![trash]);
}

#[test]
fn ambiguous_pending_removal_blocks_only_its_own_root() {
    let temp = TempDir::new().unwrap();
    let left = temp.path().join("left");
    let right = temp.path().join("right");
    fs::create_dir(&left).unwrap();
    fs::create_dir(&right).unwrap();
    fs::write(left.join("file.txt"), "left").unwrap();
    fs::write(right.join("file.txt"), "right").unwrap();
    let mut initial = manager(&temp);
    initial.init(&left).unwrap();
    initial.init(&right).unwrap();
    let child = initial
        .create(Create::new(left.clone()).named("child"))
        .unwrap();
    let child_id = marker_id(&child);
    let trash = trash_path(&child_id, &child).unwrap();
    let root = initial.root(&initial.workspace_at(&left).unwrap()).unwrap();
    initial
        .registry
        .stage_removal(
            &root,
            false,
            &[MovedRecord {
                id: child_id,
                original_path: child.clone(),
                trash_path: trash.clone(),
            }],
        )
        .unwrap();
    // Model an ambiguous crash/manual-intervention state. Recovery must not
    // guess which copy is authoritative, but it also must not brick unrelated
    // roots sharing this registry.
    fs::create_dir_all(&trash).unwrap();
    drop(initial);

    let mut recovered = manager(&temp);
    assert_eq!(recovered.describe(&right).unwrap().parent, None);
    assert!(matches!(recovered.remove(&child), Err(Error::AlreadyExists(path)) if path == trash));
    assert!(
        recovered
            .create(Create::new(right).named("still-works"))
            .is_ok()
    );
}

#[test]
fn init_recovers_pending_root_unregistration_before_restoring_a_marker() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut initial = manager(&temp);
    initial.init(&source).unwrap();
    let child = initial
        .create(Create::new(source.clone()).named("child"))
        .unwrap();
    // Open before the removal is staged, which models a second process whose
    // startup recovery has already run.
    let mut contender = manager(&temp);
    let child_id = marker_id(&child);
    let trash = trash_path(&child_id, &child).unwrap();
    let root = initial
        .root(&initial.workspace_at(&source).unwrap())
        .unwrap();
    initial
        .registry
        .stage_removal(
            &root,
            true,
            &[MovedRecord {
                id: child_id,
                original_path: child.clone(),
                trash_path: trash.clone(),
            }],
        )
        .unwrap();
    fs::create_dir_all(trash.parent().unwrap()).unwrap();
    fs::rename(&child, &trash).unwrap();
    marker::remove_regular(&source).unwrap();
    drop(initial);

    assert!(matches!(
        contender.init(&source),
        Err(Error::WorkspaceNotInitialized(path)) if path == source
    ));
    assert!(!source.join(".rift").exists());
    assert!(matches!(
        contender.list(&source),
        Err(Error::WorkspaceNotInitialized(_))
    ));
}

#[test]
fn gc_never_unregisters_an_unrelated_temporarily_absent_root() {
    let temp = TempDir::new().unwrap();
    let left = temp.path().join("left");
    let right = temp.path().join("right");
    fs::create_dir(&left).unwrap();
    fs::create_dir(&right).unwrap();
    fs::write(left.join("file.txt"), "left").unwrap();
    fs::write(right.join("file.txt"), "right").unwrap();
    let mut manager = manager(&temp);
    manager.init(&left).unwrap();
    manager.init(&right).unwrap();
    let child = manager
        .create(Create::new(left.clone()).named("child"))
        .unwrap();
    manager.remove(&child).unwrap();

    let absent = temp.path().join("right-absent");
    fs::rename(&right, &absent).unwrap();
    manager.gc().unwrap();
    fs::rename(&absent, &right).unwrap();

    let described = manager.describe(&right).unwrap();
    assert_eq!(described.path, fs::canonicalize(&right).unwrap());
    assert_eq!(described.parent, None);
    assert_eq!(
        manager.init(&right).unwrap(),
        InitOutcome::AlreadyInitialized
    );
}

#[test]
fn operations_use_the_nearest_ancestor_marker() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let nested = source.join("packages/app");
    fs::create_dir_all(&nested).unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let child = manager.create(Create::new(nested).named("nested")).unwrap();
    fs::create_dir(child.join("deep")).unwrap();

    assert_eq!(
        manager.list(source.join("packages")).unwrap(),
        vec![child.clone()]
    );
    assert_eq!(manager.ancestors(child.join("deep")).unwrap(), vec![source]);
}

#[test]
fn operations_without_a_marker_explain_how_to_initialize() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let nested = source.join("nested");
    fs::create_dir(&nested).unwrap();
    let manager = manager(&temp);

    assert!(matches!(
        manager.list(&nested),
        Err(Error::WorkspaceNotInitialized(path)) if path == nested
    ));
}

#[test]
fn init_restores_a_deleted_marker_for_an_existing_workspace() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let nested = source.join("nested");
    fs::create_dir(&nested).unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let id = fs::read_to_string(source.join(".rift")).unwrap();
    fs::remove_file(source.join(".rift")).unwrap();

    assert!(matches!(
        manager.list(&nested),
        Err(Error::MissingMarker(path)) if path == source
    ));

    manager.init(&source).unwrap();
    assert_eq!(fs::read_to_string(source.join(".rift")).unwrap(), id);
    assert!(manager.list(&nested).unwrap().is_empty());
}

struct InitializingStrategy {
    initialized: Rc<Cell<bool>>,
}

impl Strategy for InitializingStrategy {
    fn copy_directory(&self, _from: &Path, _to: &Path, _mode: CopyMode) -> Result<()> {
        unreachable!()
    }

    fn initialize_directory(
        &self,
        _path: &Path,
        _progress: &mut dyn FnMut(InitProgress),
    ) -> Result<StrategyInit> {
        self.initialized.set(true);
        Ok(StrategyInit::Converted)
    }
}

#[test]
fn init_continues_initialization_after_restoring_a_marker() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut registered = manager(&temp);
    registered.init(&source).unwrap();
    fs::remove_file(source.join(".rift")).unwrap();
    drop(registered);
    let initialized = Rc::new(Cell::new(false));
    let mut manager = Manager::with_strategy(
        temp.path().join("registry.sqlite"),
        Box::new(InitializingStrategy {
            initialized: initialized.clone(),
        }),
    )
    .unwrap();

    assert_eq!(manager.init(&source).unwrap(), InitOutcome::Converted);
    assert!(initialized.get());
    assert!(source.join(".rift").exists());
}

#[test]
fn init_registers_exactly_the_requested_directory() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let nested = source.join("nested");
    fs::create_dir(&nested).unwrap();
    run(&source, &["init"]);
    let mut manager = manager(&temp);

    manager.init(&nested).unwrap();
    assert!(!source.join(".rift").exists());
    assert!(nested.join(".rift").exists());
}

#[test]
fn create_generates_readable_names_independent_of_ulid_identity() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let destination = manager.create(Create::new(source)).unwrap();
    let name = destination.file_name().unwrap().to_str().unwrap();
    let parts = name.split('-').collect::<Vec<_>>();
    let id = fs::read_to_string(destination.join(".rift")).unwrap();
    let id = id.trim();

    assert_eq!(parts.len(), 2);
    assert!(
        parts[0]
            .chars()
            .all(|character| character.is_ascii_lowercase())
    );
    assert!(
        parts[1]
            .chars()
            .all(|character| character.is_ascii_lowercase())
    );
    assert!(Ulid::from_string(id).is_ok());
    assert_ne!(name, id);
}

#[test]
fn generated_destinations_retry_then_use_a_unique_id_suffix() {
    let temp = TempDir::new().unwrap();
    let parent = temp.path().join("storage");
    fs::create_dir(&parent).unwrap();
    fs::create_dir(parent.join("taken")).unwrap();
    let id = RiftId::from_stored("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned());
    let mut names = ["taken", "available"].into_iter();

    let retried = generated_destination_with(&parent, &id, || {
        RiftName::from_optional(Some(names.next().unwrap().to_owned())).unwrap()
    })
    .unwrap();
    assert_eq!(retried, parent.join("available"));

    let fallback = generated_destination_with(&parent, &id, || {
        RiftName::from_optional(Some("taken".to_owned())).unwrap()
    })
    .unwrap();
    assert!(
        fallback
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(&id.as_str().to_ascii_lowercase())
    );
}

#[cfg(unix)]
#[test]
fn init_and_create_reject_marker_symlinks_without_touching_their_targets() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let external = temp.path().join("external-marker");
    std::os::unix::fs::symlink(&external, source.join(".rift")).unwrap();
    let mut manager = manager(&temp);

    assert!(matches!(manager.init(&source), Err(Error::UnsafeMarker(_))));
    assert!(!external.exists());
    assert!(
        fs::symlink_metadata(source.join(".rift"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    fs::remove_file(source.join(".rift")).unwrap();
    manager.init(&source).unwrap();
    let original = fs::read_to_string(source.join(".rift")).unwrap();
    fs::remove_file(source.join(".rift")).unwrap();
    fs::write(&external, &original).unwrap();
    std::os::unix::fs::symlink(&external, source.join(".rift")).unwrap();

    assert!(matches!(
        manager.create(Create::new(source.clone()).named("unsafe-marker")),
        Err(Error::UnsafeMarker(_))
    ));
    assert_eq!(fs::read_to_string(&external).unwrap(), original);
    assert!(!child_path(&source, "unsafe-marker").exists());
}

#[test]
fn remove_trashes_a_full_subtree_and_gc_deletes_it() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();

    let first_id = marker_id(&first);
    let first_trash = trash_path(&first_id, &first).unwrap();
    let second_id = marker_id(&second);
    let second_trash = trash_path(&second_id, &second).unwrap();

    manager.remove(&first).unwrap();

    assert!(!first.exists());
    assert!(!second.exists());
    assert!(first_trash.exists());
    assert!(second_trash.exists());
    assert!(manager.list(&source).unwrap().is_empty());
    let deleted = manager.gc().unwrap();
    assert!(deleted.contains(&second_trash));
    assert!(deleted.contains(&first_trash));
    assert_eq!(deleted.len(), 2);
    assert!(!first_trash.exists());
    assert!(!second_trash.exists());
}

#[test]
fn remove_on_a_registered_root_unregisters_it_and_trashes_descendants() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    let first_id = marker_id(&first);
    let first_trash = trash_path(&first_id, &first).unwrap();
    let second_id = marker_id(&second);
    let second_trash = trash_path(&second_id, &second).unwrap();

    manager.remove(&source).unwrap();

    assert!(source.exists());
    assert!(!source.join(".rift").exists());
    assert!(!first.exists());
    assert!(!second.exists());
    assert!(first_trash.exists());
    assert!(second_trash.exists());
    assert!(matches!(
        manager.list(&source),
        Err(Error::WorkspaceNotInitialized(_))
    ));
    let deleted = manager.gc().unwrap();
    assert!(deleted.contains(&first_trash));
    assert!(deleted.contains(&second_trash));
}

#[test]
fn remove_on_a_registered_root_tolerates_missing_descendants() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    fs::remove_dir_all(&first).unwrap();

    manager.remove(&source).unwrap();

    assert!(source.exists());
    assert!(!source.join(".rift").exists());
    assert!(matches!(
        manager.list(&source),
        Err(Error::WorkspaceNotInitialized(_))
    ));
}

#[test]
fn remove_all_deletes_descendants_and_preserves_the_selected_workspace() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    let sibling = manager
        .create(Create::new(source.clone()).named("sibling"))
        .unwrap();
    let first_id = marker_id(&first);
    let first_trash = trash_path(&first_id, &first).unwrap();

    let removed = manager.remove_all(&source).unwrap();
    assert_eq!(removed[0], second);
    assert!(removed.contains(&first));
    assert!(removed.contains(&sibling));
    assert_eq!(removed.len(), 3);
    assert!(source.exists());
    assert!(!first.exists());
    assert!(first_trash.exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

#[test]
fn remove_all_preserves_a_nested_selected_rift() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager.create(Create::new(source).named("first")).unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();

    assert_eq!(manager.remove_all(&first).unwrap(), vec![second]);
    assert!(first.exists());
    assert!(manager.list(&first).unwrap().is_empty());
}

#[test]
fn remove_refuses_a_subtree_with_an_unlinked_move() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    fs::rename(&second, temp.path().join("moved")).unwrap();

    assert!(matches!(manager.remove(&first), Err(Error::MissingRift(_))));
    assert!(first.exists());
}

#[test]
fn gc_removes_trashed_entries() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    let first_id = marker_id(&first);
    let second_id = marker_id(&second);
    let first_trash = trash_path(&first_id, &first).unwrap();
    let second_trash = trash_path(&second_id, &second).unwrap();
    manager.remove(&first).unwrap();

    let deleted = manager.gc().unwrap();
    assert!(deleted.contains(&second_trash));
    assert!(deleted.contains(&first_trash));
    assert_eq!(deleted.len(), 2);
    assert!(manager.list(&source).unwrap().is_empty());
    // Emptied `.trash` containers are pruned along with their last entry.
    assert!(!first_trash.parent().unwrap().exists());
    assert!(!second_trash.parent().unwrap().exists());
}

#[test]
fn gc_has_no_effect_on_active_rifts() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    assert!(manager.gc().unwrap().is_empty());
    assert_eq!(manager.ancestors(&second).unwrap(), vec![first, source]);
}

#[test]
fn gc_preserves_active_entries_deleted_outside_rift() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    fs::remove_dir_all(&first).unwrap();
    fs::remove_dir_all(&second).unwrap();

    assert!(manager.gc().unwrap().is_empty());
    assert_eq!(manager.list(&source).unwrap(), vec![first]);
    assert!(matches!(
        manager.describe(&second),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
}

#[test]
fn gc_preserves_missing_active_parent_with_an_existing_descendant() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let first = manager
        .create(Create::new(source.clone()).named("first"))
        .unwrap();
    let second = manager
        .create(Create::new(first.clone()).named("second"))
        .unwrap();
    fs::remove_dir_all(&first).unwrap();

    assert!(manager.gc().unwrap().is_empty());
    assert_eq!(manager.list(&source).unwrap(), vec![first]);
    assert_eq!(
        manager.ancestors(&second).unwrap(),
        vec![source.parent().unwrap().join(".rifts/app/first"), source]
    );
}

#[test]
fn git_copy_detaches_head_and_preserves_dirty_state() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    run(&source, &["init"]);
    run(&source, &["config", "user.email", "test@example.com"]);
    run(&source, &["config", "user.name", "Test"]);
    run(&source, &["add", "file.txt"]);
    run(&source, &["commit", "-m", "initial"]);
    fs::write(source.join("file.txt"), "changed").unwrap();
    run(&source, &["add", "file.txt"]);
    fs::write(source.join("untracked.txt"), "new").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let destination = manager
        .create(Create::new(source.clone()).named("git"))
        .unwrap();

    let source_commit = Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()
        .unwrap();
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["symbolic-ref", "-q", "HEAD"])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        fs::read_to_string(destination.join(".git/HEAD")).unwrap(),
        format!(
            "{}\n",
            String::from_utf8_lossy(&source_commit.stdout).trim()
        )
    );
    let staged = Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&staged.stdout).contains("file.txt"));
    assert!(destination.join("untracked.txt").exists());
    let status = Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["status", "--porcelain", "--", ".rift"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());
}

#[test]
fn git_copy_peels_symbolic_tag_heads_to_commits() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    run(&source, &["init"]);
    run(&source, &["config", "user.email", "test@example.com"]);
    run(&source, &["config", "user.name", "Test"]);
    run(&source, &["add", "file.txt"]);
    run(&source, &["commit", "-m", "initial"]);
    run(&source, &["tag", "-a", "release", "-m", "release"]);
    run(&source, &["symbolic-ref", "HEAD", "refs/tags/release"]);
    let expected = Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()
        .unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let destination = manager
        .create(Create::new(source).named("tag-head"))
        .unwrap();

    assert_eq!(
        fs::read_to_string(destination.join(".git/HEAD")).unwrap(),
        format!("{}\n", String::from_utf8_lossy(&expected.stdout).trim())
    );
}

#[test]
fn filtered_git_copy_preserves_refs_named_like_build_artifacts() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    run(&source, &["init"]);
    run(&source, &["config", "user.email", "test@example.com"]);
    run(&source, &["config", "user.name", "Test"]);
    run(&source, &["add", "file.txt"]);
    run(&source, &["commit", "-m", "initial"]);
    let expected = Command::new("git")
        .arg("-C")
        .arg(&source)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let expected = String::from_utf8(expected.stdout).unwrap();
    fs::create_dir_all(source.join(".git/refs/heads/build")).unwrap();
    fs::write(source.join(".git/refs/heads/build/test"), &expected).unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    let destination = manager
        .create(Create::new(source).named("build-ref"))
        .unwrap();
    let observed = Command::new("git")
        .arg("-C")
        .arg(&destination)
        .args(["rev-parse", "refs/heads/build/test"])
        .output()
        .unwrap();

    assert!(observed.status.success());
    assert_eq!(observed.stdout, expected.as_bytes());
}

#[test]
fn create_requires_an_initialized_workspace() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);

    assert!(matches!(
        manager.create(Create::new(source.clone()).named("unsafe")),
        Err(Error::WorkspaceNotInitialized(_))
    ));
    assert!(!source.join(".rift").exists());
}

#[test]
fn unsafe_git_states_are_rejected_after_initialization() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    run(&source, &["init"]);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();

    for state in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
        "HEAD.lock",
    ] {
        let marker = source.join(".git").join(state);
        if state.starts_with("rebase-") {
            fs::create_dir(&marker).unwrap();
        } else {
            fs::write(&marker, "commit").unwrap();
        }
        let name = format!("unsafe-{state}");
        let expected = child_path(&source, &name);

        let error = manager
            .create(Create::new(source.clone()).named(name))
            .unwrap_err();

        assert!(matches!(error, Error::UnsafeGit(message) if message.contains(state)));
        assert!(!expected.exists());
        assert!(manager.list(&source).unwrap().is_empty());
        if marker.is_dir() {
            fs::remove_dir(&marker).unwrap();
        } else {
            fs::remove_file(&marker).unwrap();
        }
    }
}

#[test]
fn linked_git_worktree_source_is_rejected_after_initialization() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    run(&source, &["init"]);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    fs::remove_dir_all(source.join(".git")).unwrap();
    fs::write(source.join(".git"), "gitdir: ../linked/.git").unwrap();

    let error = manager
        .create(Create::new(source.clone()).named("linked-worktree"))
        .unwrap_err();

    assert!(matches!(error, Error::UnsafeGit(message) if message.contains("linked")));
    assert!(!child_path(&source, "linked-worktree").exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

struct PartialFailureStrategy;

impl Strategy for PartialFailureStrategy {
    fn copy_directory(&self, _from: &Path, to: &Path, _mode: CopyMode) -> Result<()> {
        fs::create_dir(to)?;
        fs::write(to.join("copied-before-failure.txt"), "partial")?;
        fs::create_dir(to.join("nested"))?;
        fs::write(to.join("nested/file.txt"), "partial")?;
        Err(Error::CowUnavailable("partial failure".into()))
    }
}

#[test]
fn partial_copy_failure_removes_child_and_registry_row() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = Manager::with_strategy(
        temp.path().join("registry.sqlite"),
        Box::new(PartialFailureStrategy),
    )
    .unwrap();
    manager.init(&source).unwrap();
    let expected = child_path(&source, "partial");

    let error = manager
        .create(Create::new(source.clone()).named("partial"))
        .unwrap_err();

    assert!(matches!(error, Error::CowUnavailable(message) if message == "partial failure"));
    assert!(source.join(".rift").exists());
    assert!(!expected.exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn unreadable_source_file_failure_removes_child_and_registry_row() {
    if running_as_root() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let secret = source.join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

    let result = manager.create(Create::new(source.clone()).named("unreadable-file"));
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let error = result.unwrap_err();

    assert!(matches!(error, Error::Io(_)));
    assert!(source.join(".rift").exists());
    assert!(!child_path(&source, "unreadable-file").exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn unreadable_source_directory_failure_removes_child_and_registry_row() {
    if running_as_root() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let secret = source.join("secret");
    fs::create_dir(&secret).unwrap();
    fs::write(secret.join("file.txt"), "secret").unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

    let result = manager.create(Create::new(source.clone()).named("unreadable-directory"));
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();

    assert!(matches!(error, Error::Walk(_) | Error::Io(_)));
    assert!(source.join(".rift").exists());
    assert!(!child_path(&source, "unreadable-directory").exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn unwritable_destination_parent_failure_leaves_no_child_or_registry_row() {
    if running_as_root() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let parent = temp.path().join("readonly");
    fs::create_dir(&parent).unwrap();
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();

    let result = manager.create(
        Create::new(source.clone())
            .named("blocked")
            .with_storage(Some(parent.clone())),
    );
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();

    assert!(matches!(error, Error::Io(_)));
    assert!(source.join(".rift").exists());
    assert!(!parent.join("blocked").exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

#[test]
fn unavailable_cow_does_not_create_a_child() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = Manager::with_strategy(
        temp.path().join("registry.sqlite"),
        Box::new(FailureStrategy),
    )
    .unwrap();
    manager.init(&source).unwrap();

    assert!(matches!(
        manager.create(Create::new(source.clone()).named("failure")),
        Err(Error::CowUnavailable(_))
    ));
    assert!(source.join(".rift").exists());
    assert!(manager.list(&source).unwrap().is_empty());
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions and only reads the process identity.
    unsafe { libc::geteuid() == 0 }
}

fn run(path: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn describe_reports_root_child_and_dangling_parent() {
    let temp = TempDir::new().unwrap();
    let source = source(&temp);
    let mut manager = manager(&temp);
    manager.init(&source).unwrap();
    let child = manager
        .create(Create::new(source.clone()).named("child"))
        .unwrap();

    let root = manager.describe(&source).unwrap();
    assert_eq!(root.path, source);
    assert_eq!(root.parent, None);

    let described = manager.describe(&child).unwrap();
    assert_eq!(described.path, child);
    assert_eq!(described.parent, Some(source.clone()));

    // The registry's ON DELETE CASCADE means a dangling parent cannot arise
    // through normal operations -- simulate external corruption by deleting the
    // root row over a raw connection (foreign_keys defaults to OFF there). The
    // error must name the real problem (a dangling parent reference), not
    // claim the child itself is unmanaged.
    let database = rusqlite::Connection::open(temp.path().join("registry.sqlite")).unwrap();
    database
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .unwrap();
    database
        .execute("DELETE FROM rift WHERE parent_id IS NULL", [])
        .unwrap();
    drop(database);
    let error = manager.describe(&child).unwrap_err();
    assert!(matches!(error, Error::DanglingParent { .. }), "{error}");
    assert!(error.to_string().contains("missing from the registry"));
}
