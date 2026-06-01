use crate::{Error, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

pub(crate) fn check_source(path: &Path) -> Result<bool> {
    let git = path.join(".git");
    if !git.exists() {
        return Ok(false);
    }
    if !git.is_dir() {
        return Err(Error::UnsafeGit(
            "linked Git worktree sources are not supported".into(),
        ));
    }

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
        if git.join(state).exists() {
            return Err(Error::UnsafeGit(format!("Git state in progress: {state}")));
        }
    }
    Ok(true)
}

pub(crate) fn hide_marker(path: &Path) -> Result<()> {
    let info = path.join(".git").join("info");
    fs::create_dir_all(&info)?;
    let exclude = info.join("exclude");
    let existing = if exclude.exists() {
        fs::read_to_string(&exclude)?
    } else {
        String::new()
    };
    if existing.lines().any(|line| line.trim() == "/.rift") {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(exclude, format!("{existing}{separator}/.rift\n"))?;
    Ok(())
}

pub(crate) fn detach_destination(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    fs::write(path.join(".git").join("HEAD"), format!("{commit}\n"))?;
    Ok(())
}
