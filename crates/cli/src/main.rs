use clap::{Parser, Subcommand};
use rift::{Create, Manager};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rift")]
struct Cli {
    #[arg(long, hide = true)]
    database: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    let mut manager = match cli.database {
        Some(path) => Manager::open(path)?,
        None => Manager::open_default()?,
    };
    match cli.command {
        Command::Init { at } => {
            let at = at.unwrap_or(std::env::current_dir()?);
            let at = std::fs::canonicalize(at)?;
            let initialized_from_inside = std::env::current_dir()?.starts_with(&at);
            if let Some(backup) = manager.init(&at)? {
                println!(
                    "initialized btrfs subvolume; original workspace retained at {}",
                    backup.display()
                );
                if initialized_from_inside {
                    println!(
                        "run `cd {}` to enter the initialized workspace",
                        at.display()
                    );
                }
            }
        }
        Command::Create { from, name, into } => {
            println!(
                "{}",
                manager
                    .create(Create {
                        from: from.unwrap_or(std::env::current_dir()?),
                        name,
                        into,
                    })?
                    .display()
            );
        }
        Command::Remove { at, all } => {
            let at = at.unwrap_or(std::env::current_dir()?);
            if all {
                for path in manager.remove_all(at)? {
                    println!("{}", path.display());
                }
            } else {
                manager.remove(at)?;
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
