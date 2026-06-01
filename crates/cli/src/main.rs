use clap::{Parser, Subcommand, ValueEnum};
use rift::{Create, InitProgress, Manager};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rift")]
struct Cli {
    #[arg(long, hide = true)]
    database: Option<PathBuf>,
    #[arg(long, hide = true, global = true)]
    shell_cwd: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Shell {
    Bash,
    Zsh,
}

#[derive(Subcommand)]
enum Command {
    ShellInit {
        #[arg(value_enum)]
        shell: Shell,
    },
    Init {
        at: Option<PathBuf>,
        #[arg(long)]
        here: bool,
    },
    Create {
        from: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        into: Option<PathBuf>,
    },
    Remove {
        at: Option<PathBuf>,
        #[arg(long)]
        all: bool,
    },
    List {
        of: Option<PathBuf>,
    },
    Ancestors {
        of: Option<PathBuf>,
    },
    Gc,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("rift: {}", error_message(&error));
        std::process::exit(1);
    }
}

fn error_message(error: &rift::Error) -> String {
    match error {
        rift::Error::InitializationRequired(path) => format!(
            "workspace is not a btrfs subvolume: {}; run `rift init` in the root folder first",
            path.display()
        ),
        rift::Error::WorkspaceNotInitialized(path) => format!(
            "no .rift file found from: {}; run `rift init` in the root folder first",
            path.display()
        ),
        rift::Error::MissingMarker(path) => format!(
            "the .rift file is missing at: {}; run `rift init {}` to restore it",
            path.display(),
            path.display()
        ),
        _ => error.to_string(),
    }
}

fn run() -> rift::Result<()> {
    let cli = Cli::parse();
    if let Command::ShellInit { shell } = &cli.command {
        print_shell_init(*shell);
        return Ok(());
    }
    let mut manager = match cli.database {
        Some(path) => Manager::open(path)?,
        None => Manager::open_default()?,
    };
    match cli.command {
        Command::ShellInit { .. } => unreachable!(),
        Command::Init { at, here } => {
            let requested = std::fs::canonicalize(at.unwrap_or(std::env::current_dir()?))?;
            let (at, existing, missing_marker) = init_target(&manager, &requested, here)?;
            eprintln!("using workspace at {}", at.display());
            let mut imported_entries = 0;
            let mut reported_entries = 0;
            let converted = manager.init_with_progress(&at, |progress| {
                if progress == InitProgress::CreatingSubvolume {
                    eprintln!("first time init can be slow. creating new rifts will be instant");
                }
                match progress {
                    InitProgress::ImportedEntries { entries } => {
                        imported_entries = entries;
                        if entries >= reported_entries + 1000 {
                            eprintln!("imported {entries} entries");
                            reported_entries = entries;
                        }
                    }
                    InitProgress::ActivatingWorkspace => {
                        if imported_entries > reported_entries {
                            eprintln!("imported {imported_entries} entries");
                        }
                        eprintln!("{}", init_progress_message(progress));
                    }
                    _ => eprintln!("{}", init_progress_message(progress)),
                }
            })?;
            if converted {
                let initialized_from_inside = std::env::current_dir()?.starts_with(&at);
                eprintln!("initialized btrfs subvolume at {}", at.display());
                if initialized_from_inside {
                    if cli.shell_cwd {
                        println!("{}", at.display());
                    } else {
                        eprintln!(
                            "run `cd {}` to enter the initialized workspace",
                            at.display()
                        );
                    }
                }
            } else if let Some(root) = missing_marker {
                eprintln!(
                    "restored missing .rift file for initialized workspace at {}",
                    root.display()
                );
            } else if let Some(existing) = existing {
                eprintln!("rift is already initialized at {}", existing.display());
            }
        }
        Command::Create { from, name, into } => {
            let destination = manager.create(Create {
                from: from.unwrap_or(std::env::current_dir()?),
                name,
                into,
            })?;
            if cli.shell_cwd {
                eprintln!("created {}", destination.display());
            }
            println!("{}", destination.display());
        }
        Command::Remove { at, all } => {
            let at = manager.workspace(at.unwrap_or(std::env::current_dir()?))?;
            let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
            if all {
                let removed = manager.remove_all(&at)?;
                for path in &removed {
                    if cli.shell_cwd {
                        eprintln!("removed {}", path.display());
                    } else {
                        println!("{}", path.display());
                    }
                }
                if cli.shell_cwd && removed.iter().any(|path| cwd.starts_with(path)) {
                    println!("{}", at.display());
                }
            } else {
                let ancestors = manager.ancestors(&at)?;
                let unregistering_root = ancestors.is_empty();
                let destination = if cli.shell_cwd && cwd.starts_with(&at) {
                    if unregistering_root {
                        Some(at.clone())
                    } else {
                        ancestors.into_iter().next()
                    }
                } else {
                    None
                };
                manager.remove(&at)?;
                if cli.shell_cwd {
                    if unregistering_root {
                        eprintln!("unregistered {}", at.display());
                    } else {
                        eprintln!("removed {}", at.display());
                    }
                    if let Some(destination) = destination {
                        println!("{}", destination.display());
                    }
                }
            }
        }
        Command::List { of } => {
            for path in manager.list(of.unwrap_or(std::env::current_dir()?))? {
                println!("{}", path.display());
            }
        }
        Command::Ancestors { of } => {
            for path in manager.ancestors(of.unwrap_or(std::env::current_dir()?))? {
                println!("{}", path.display());
            }
        }
        Command::Gc => {
            for path in manager.gc()? {
                println!("{}", path.display());
            }
        }
    }
    Ok(())
}

fn init_target(
    manager: &Manager,
    requested: &std::path::Path,
    here: bool,
) -> rift::Result<(PathBuf, Option<PathBuf>, Option<PathBuf>)> {
    if here {
        return Ok((requested.to_path_buf(), None, None));
    }
    match manager.workspace(requested) {
        Ok(root) => Ok((root.clone(), Some(root), None)),
        Err(rift::Error::MissingMarker(root)) => Ok((root.clone(), None, Some(root))),
        Err(rift::Error::WorkspaceNotInitialized(_)) => Ok((git_root(requested), None, None)),
        Err(error) => Err(error),
    }
}

fn git_root(path: &std::path::Path) -> PathBuf {
    path.ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(path)
        .to_path_buf()
}

fn init_progress_message(progress: InitProgress) -> &'static str {
    match progress {
        InitProgress::CreatingSubvolume => "creating btrfs subvolume",
        InitProgress::ImportingWorkspace => "importing existing workspace",
        InitProgress::ImportedEntries { .. } => "importing existing workspace",
        InitProgress::ActivatingWorkspace => "activating initialized workspace",
        InitProgress::RemovingOriginal => "removing original directory after clean swap",
        InitProgress::RestoringMarker => "restoring missing .rift file",
        InitProgress::RegisteringWorkspace => "registering workspace",
    }
}

fn print_shell_init(_shell: Shell) {
    let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rift"));
    let executable = shell_quote(&executable.to_string_lossy());
    println!(
        r#"rift() {{
  case "${{1-}}" in
    init|create|remove)
      local __rift_cwd
      __rift_cwd="$({executable} --shell-cwd "$@")" || return $?
      if [ -n "$__rift_cwd" ]; then
        builtin cd -- "$__rift_cwd" || return $?
      fi
      ;;
    *)
      {executable} "$@"
      ;;
  esac
}}"#,
    );
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn initialization_guidance_is_rendered_by_the_cli() {
        let path = PathBuf::from("/tmp/app");

        assert_eq!(
            error_message(&rift::Error::WorkspaceNotInitialized(path.clone())),
            "no .rift file found from: /tmp/app; run `rift init` in the root folder first"
        );
        assert_eq!(
            error_message(&rift::Error::MissingMarker(path)),
            "the .rift file is missing at: /tmp/app; run `rift init /tmp/app` to restore it"
        );
    }

    #[test]
    fn init_target_selects_git_root_unless_here_is_requested() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("app");
        let nested = root.join("nested");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir(&nested).unwrap();
        let manager = Manager::open(temp.path().join("rift.sqlite")).unwrap();

        assert_eq!(init_target(&manager, &nested, false).unwrap().0, root);
        assert_eq!(init_target(&manager, &nested, true).unwrap().0, nested);
    }

    #[test]
    fn init_progress_is_formatted_by_the_cli() {
        assert_eq!(
            init_progress_message(InitProgress::ImportingWorkspace),
            "importing existing workspace"
        );
        assert_eq!(
            init_progress_message(InitProgress::RegisteringWorkspace),
            "registering workspace"
        );
        assert_eq!(
            init_progress_message(InitProgress::ImportedEntries { entries: 42 }),
            "importing existing workspace"
        );
    }
}
