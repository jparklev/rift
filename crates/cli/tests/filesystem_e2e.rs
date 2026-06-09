#[cfg(target_os = "linux")]
mod support;

#[cfg(target_os = "linux")]
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use support::filesystem::{
    CliFixture, assert_different_filesystems, assert_registry_empty,
    supported_linux_filesystem_tests_required, unsupported_linux_filesystem_tests_required,
};

#[cfg(target_os = "linux")]
#[test]
fn supported_filesystem_cli_round_trip() {
    if !supported_linux_filesystem_tests_required() {
        return;
    }
    let fixture = CliFixture::current_filesystem(".rift-cli-supported-");
    let source = fixture.root().join("source");
    create_workspace(&source);

    let init = fixture.success(
        fixture.root(),
        [os("init"), os(source.as_os_str()), os("--here")],
    );
    assert!(init.stdout.is_empty());
    assert!(source.join(".rift").exists());

    let child = fixture
        .success(&source, ["create", "--name", "child"])
        .single_stdout_path();
    assert_eq!(child, fixture.root().join(".rifts/source/child"));
    assert_workspace_copy(&child);

    let custom_parent = fixture.root().join("custom-storage");
    let custom = fixture
        .success(
            &source,
            [
                os("create"),
                os("--name"),
                os("custom"),
                os("--into"),
                os(custom_parent.as_os_str()),
            ],
        )
        .single_stdout_path();
    assert_eq!(custom, custom_parent.join("custom"));
    assert_workspace_copy(&custom);

    assert_paths_unordered(
        fixture.success(&source, ["list"]).stdout_paths(),
        &[child.clone(), custom.clone()],
    );
    assert_eq!(
        fixture
            .success(&source, [os("ancestors"), os(custom.as_os_str())])
            .stdout_paths(),
        vec![source.clone()]
    );

    let external = tempfile::TempDir::new().unwrap();
    assert_different_filesystems(&source, external.path());
    let external_parent = external.path().join("external-storage");
    let failed = fixture.failure(
        &source,
        [
            os("create"),
            os("--name"),
            os("external"),
            os("--into"),
            os(external_parent.as_os_str()),
        ],
    );
    assert!(failed.stderr.contains("copy-on-write cloning unavailable"));
    assert!(!external_parent.join("external").exists());

    let remove = fixture.success(&source, [os("remove"), os(child.as_os_str())]);
    assert!(remove.stdout.is_empty());
    assert!(!child.exists());
    assert_eq!(
        fixture.success(&source, ["list"]).stdout_paths(),
        vec![custom.clone()]
    );

    // Removal reclaims trash immediately, so gc has nothing left to do.
    let trash = fixture.root().join(".rifts/source/.trash");
    assert!(!trash.exists() || fs::read_dir(&trash).unwrap().next().is_none());
    assert!(fixture.success(&source, ["gc"]).stdout_paths().is_empty());
    assert!(!fixture.default_database().exists());
}

#[cfg(target_os = "linux")]
#[test]
fn unsupported_filesystem_cli_fails_closed() {
    if !unsupported_linux_filesystem_tests_required() {
        return;
    }
    let fixture = CliFixture::current_filesystem(".rift-cli-unsupported-");
    let source = fixture.root().join("source");
    create_workspace(&source);

    let init = fixture.failure(
        fixture.root(),
        [os("init"), os(source.as_os_str()), os("--here")],
    );
    assert!(init.stdout.is_empty());
    assert!(init.stderr.contains("copy-on-write cloning unavailable"));
    assert!(!source.join(".rift").exists());
    assert!(!fixture.root().join(".rifts").exists());
    assert_no_reflink_probe_files(&source);
    assert_registry_empty(fixture.database());

    let create = fixture.failure(&source, ["create", "--name", "child"]);
    assert!(create.stderr.contains("no initialized workspace found"));
    assert!(!fixture.root().join(".rifts/source/child").exists());
    assert!(!fixture.default_database().exists());
    assert_registry_empty(fixture.database());
}

#[cfg(target_os = "linux")]
fn create_workspace(source: &Path) {
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("nested/file.txt"), "hello from cli e2e").unwrap();
    fs::write(source.join("untracked.txt"), "kept").unwrap();
}

#[cfg(target_os = "linux")]
fn assert_workspace_copy(path: &Path) {
    assert_eq!(
        fs::read_to_string(path.join("nested/file.txt")).unwrap(),
        "hello from cli e2e"
    );
    assert_eq!(
        fs::read_to_string(path.join("untracked.txt")).unwrap(),
        "kept"
    );
    assert!(path.join(".rift").exists());
}

#[cfg(target_os = "linux")]
fn assert_no_reflink_probe_files(path: &Path) {
    assert!(
        fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .all(|name| !name.to_string_lossy().starts_with(".rift-reflink-probe"))
    );
}

#[cfg(target_os = "linux")]
fn assert_paths_unordered(mut actual: Vec<PathBuf>, expected: &[PathBuf]) {
    let mut expected = expected.to_vec();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[cfg(target_os = "linux")]
fn os(arg: impl AsRef<OsStr>) -> OsString {
    arg.as_ref().to_os_string()
}
