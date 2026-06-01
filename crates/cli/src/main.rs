use clap::{Parser, Subcommand, ValueEnum};
use rift::{Create, Manager};
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
        eprintln!("rift: {error}");
        std::process::exit(1);
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
        Command::Init { at } => {
            let at = at.unwrap_or(std::env::current_dir()?);
            let at = std::fs::canonicalize(at)?;
            let initialized_from_inside = std::env::current_dir()?.starts_with(&at);
            if let Some(backup) = manager.init(&at)? {
                eprintln!(
                    "initialized btrfs subvolume; original workspace retained at {}",
                    backup.display()
                );
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
            let at = std::fs::canonicalize(at.unwrap_or(std::env::current_dir()?))?;
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
                let destination = if cli.shell_cwd && cwd.starts_with(&at) {
                    manager.ancestors(&at)?.into_iter().next()
                } else {
                    None
                };
                manager.remove(&at)?;
                if cli.shell_cwd {
                    eprintln!("removed {}", at.display());
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

fn print_shell_init(_shell: Shell) {
    println!(
        r#"rift() {{
  case "${{1-}}" in
    init|create|remove)
      local __rift_cwd
      __rift_cwd="$(command rift --shell-cwd "$@")" || return $?
      if [ -n "$__rift_cwd" ]; then
        builtin cd -- "$__rift_cwd" || return $?
      fi
      ;;
    *)
      command rift "$@"
      ;;
  esac
}}"#
    );
}
