use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Decides which entries a filtered clone omits.
///
/// Filtered clones drop heavyweight regenerable artifacts (dependency and build
/// directories) identified by name. To stay correct for version control, a
/// name-matched entry is *never* dropped when the source repository's own index
/// tracks a path at or under it: a filtered clone must never delete committed
/// files, which would otherwise surface as spurious deletions in any later diff
/// of the cloned workspace.
///
/// The guarantee covers the source repository's own tracked files. Files tracked
/// inside a nested submodule (whose contents live in that submodule's own index)
/// are not inspected; a submodule that commits files under an artifact-named
/// directory is governed by the name filter alone.
#[derive(Clone, Debug, Default)]
pub(crate) struct CopyFilter {
    /// Tracked paths relative to the source root, case-folded when `ignore_case`.
    tracked: BTreeSet<PathBuf>,
    ignore_case: bool,
}

impl CopyFilter {
    /// A name-only filter with no Git awareness, for plain directories.
    #[cfg(test)]
    pub(crate) fn unaware() -> Self {
        Self::default()
    }

    /// A filter that protects every Git-tracked path under `source`.
    pub(crate) fn for_source(source: &Path) -> Self {
        let (paths, ignore_case) = tracked_paths(source);
        Self::from_tracked(paths, ignore_case)
    }

    fn from_tracked(paths: BTreeSet<PathBuf>, ignore_case: bool) -> Self {
        let tracked = paths.iter().map(|path| fold(path, ignore_case)).collect();
        Self { tracked, ignore_case }
    }

    pub(crate) fn excludes(&self, path: &Path) -> bool {
        name_excluded(path) && !self.protects(path)
    }

    /// True when `path` is itself tracked or is an ancestor of a tracked path.
    /// Tracked paths that share `path` as a prefix sort contiguously from
    /// `path`, so the first entry at or after `path` settles the question.
    /// Comparisons run in the case-folded space when the repo ignores case, so
    /// an index casing that differs from the on-disk casing still protects the
    /// committed file.
    fn protects(&self, path: &Path) -> bool {
        let key = fold(path, self.ignore_case);
        self.tracked
            .range(key.clone()..)
            .next()
            .is_some_and(|candidate| candidate.starts_with(&key))
    }
}

/// ASCII-lowercase every byte of `path` when `ignore_case`, matching how Git
/// folds paths on a case-insensitive filesystem. Non-ASCII bytes are preserved.
fn fold(path: &Path, ignore_case: bool) -> PathBuf {
    if !ignore_case {
        return path.to_path_buf();
    }
    PathBuf::from(path.to_string_lossy().to_ascii_lowercase())
}

fn name_excluded(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect::<Vec<_>>();

    parts.iter().any(|part| excludes_component(part))
        || parts
            .windows(2)
            .any(|parts| matches_yarn_artifact(parts[0], parts[1]))
}

fn excludes_component(part: &OsStr) -> bool {
    [
        "node_modules",
        ".pnpm-store",
        "target",
        ".venv",
        "venv",
        ".tox",
        ".nox",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".next",
        ".nuxt",
        ".svelte-kit",
        ".turbo",
        ".vite",
        ".parcel-cache",
        ".cache",
        "dist",
        "build",
        "coverage",
    ]
    .into_iter()
    .any(|excluded| part == excluded)
}

fn matches_yarn_artifact(first: &OsStr, second: &OsStr) -> bool {
    first == ".yarn"
        && ["cache", "unplugged", "install-state.gz", "build-state.yml"]
            .into_iter()
            .any(|artifact| second == artifact)
}

/// Collect the source repository's tracked paths and whether it folds case.
/// Prefer libgit2; if it cannot read what is nonetheless a Git repository
/// (e.g. an index extension it does not understand), fall back to the `git`
/// CLI so a real repository never silently degrades to name-only filtering.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tracked_paths(source: &Path) -> (BTreeSet<PathBuf>, bool) {
    use std::os::unix::ffi::OsStrExt;

    match git2::Repository::open(source) {
        Ok(repository) => {
            let ignore_case = repository
                .config()
                .and_then(|config| config.get_bool("core.ignorecase"))
                .unwrap_or(false);
            let Ok(index) = repository.index() else {
                return tracked_paths_cli(source);
            };
            let mut tracked = BTreeSet::new();
            for entry in index.iter() {
                if !entry.path.is_empty() {
                    tracked.insert(PathBuf::from(OsStr::from_bytes(&entry.path)));
                }
            }
            (tracked, ignore_case)
        }
        Err(_) if source.join(".git").exists() => tracked_paths_cli(source),
        Err(_) => (BTreeSet::new(), false),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tracked_paths_cli(source: &Path) -> (BTreeSet<PathBuf>, bool) {
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(source)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
    };
    let ignore_case = git(&["config", "core.ignorecase"])
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "true")
        .unwrap_or(false);
    let mut tracked = BTreeSet::new();
    if let Some(output) = git(&["ls-files", "-z"]) {
        for raw in output.stdout.split(|byte| *byte == 0).filter(|s| !s.is_empty()) {
            tracked.insert(PathBuf::from(OsStr::from_bytes(raw)));
        }
    }
    (tracked, ignore_case)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn tracked_paths(_source: &Path) -> (BTreeSet<PathBuf>, bool) {
    (BTreeSet::new(), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(paths: &[&str], ignore_case: bool) -> CopyFilter {
        CopyFilter::from_tracked(paths.iter().map(PathBuf::from).collect(), ignore_case)
    }

    #[test]
    fn excludes_artifacts_at_any_depth() {
        let filter = CopyFilter::unaware();

        assert!(filter.excludes(Path::new("packages/app/node_modules/react/index.js")));
        assert!(filter.excludes(Path::new("packages/app/.yarn/cache/react.zip")));
        assert!(!filter.excludes(Path::new("packages/app/package-lock.json")));
    }

    #[test]
    fn tracked_paths_are_never_excluded() {
        let filter = tracked(&["dist/keep.txt"], false);

        // The tracked file and its enclosing directory are both protected.
        assert!(!filter.excludes(Path::new("dist")));
        assert!(!filter.excludes(Path::new("dist/keep.txt")));
        // Untracked siblings under the same directory are still dropped.
        assert!(filter.excludes(Path::new("dist/scratch.txt")));
        // A directory with the same name but no tracked content is dropped.
        assert!(filter.excludes(Path::new("packages/dist")));
        assert!(filter.excludes(Path::new("node_modules")));
    }

    #[test]
    fn case_insensitive_repo_protects_tracked_path_despite_casing_drift() {
        // Index records `Dist/keep.txt`; the on-disk directory enumerated by the
        // walk is `dist`. On a case-insensitive repo these are the same file, so
        // the committed file must be protected.
        let folding = tracked(&["Dist/keep.txt"], true);
        assert!(!folding.excludes(Path::new("dist")));
        assert!(!folding.excludes(Path::new("dist/keep.txt")));

        // Case-sensitive (e.g. Linux) keeps strict byte comparison: a `Dist`
        // index entry does not protect a distinct on-disk `dist` directory.
        let strict = tracked(&["Dist/keep.txt"], false);
        assert!(strict.excludes(Path::new("dist")));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn cli_fallback_reads_tracked_paths_and_ignorecase() {
        use std::process::Command;

        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        git(&["config", "core.ignorecase", "true"]);
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/keep.txt"), "x").unwrap();
        git(&["add", "-A"]);

        let (tracked, ignore_case) = tracked_paths_cli(root);
        assert!(ignore_case);
        assert!(tracked.contains(&PathBuf::from("dist/keep.txt")));
    }
}
