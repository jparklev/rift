use rusqlite::Connection;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::{Builder, TempDir};

#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn stdout_paths(&self) -> Vec<PathBuf> {
        self.stdout.lines().map(PathBuf::from).collect()
    }

    pub fn single_stdout_path(&self) -> PathBuf {
        let paths = self.stdout_paths();
        assert_eq!(paths.len(), 1, "expected one stdout path, got {paths:?}");
        paths.into_iter().next().unwrap()
    }
}

pub struct CliFixture {
    temp: TempDir,
    home: PathBuf,
    xdg_data_home: PathBuf,
    database: PathBuf,
    binary: PathBuf,
}

impl CliFixture {
    pub fn current_filesystem(prefix: &str) -> Self {
        let temp = Builder::new()
            .prefix(prefix)
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let home = temp.path().join("home");
        let xdg_data_home = temp.path().join("xdg-data");
        let database = temp.path().join("registry.sqlite");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg_data_home).unwrap();

        Self {
            temp,
            home,
            xdg_data_home,
            database,
            binary: PathBuf::from(env!("CARGO_BIN_EXE_rift")),
        }
    }

    pub fn root(&self) -> &Path {
        self.temp.path()
    }

    pub fn database(&self) -> &Path {
        &self.database
    }

    pub fn default_database(&self) -> PathBuf {
        self.xdg_data_home.join("rift/rift.sqlite")
    }

    pub fn success<I, S>(&self, cwd: &Path, args: I) -> CommandOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, output) = self.run(cwd, args);
        assert!(
            output.status.success(),
            "expected success for `{}`\nstdout:\n{}\nstderr:\n{}",
            command_line(&args),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        command_output(output)
    }

    pub fn failure<I, S>(&self, cwd: &Path, args: I) -> CommandOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, output) = self.run(cwd, args);
        assert!(
            !output.status.success(),
            "expected failure for `{}`\nstdout:\n{}\nstderr:\n{}",
            command_line(&args),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        command_output(output)
    }

    fn run<I, S>(&self, cwd: &Path, args: I) -> (Vec<OsString>, Output)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect::<Vec<_>>();
        let output = Command::new(&self.binary)
            .arg("--database")
            .arg(&self.database)
            .args(&args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", &self.xdg_data_home)
            .output()
            .unwrap();
        (args, output)
    }
}

pub fn supported_linux_filesystem_tests_required() -> bool {
    ["RIFT_REQUIRE_BTRFS_TESTS", "RIFT_REQUIRE_REFLINK_TESTS"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some())
}

pub fn unsupported_linux_filesystem_tests_required() -> bool {
    std::env::var_os("RIFT_REQUIRE_UNSUPPORTED_LINUX_TESTS").is_some()
}

pub fn assert_registry_empty(path: &Path) {
    let database = Connection::open(path).unwrap();
    assert_eq!(row_count(&database, "rift"), 0);
    assert_eq!(row_count(&database, "trash"), 0);
}

pub fn assert_different_filesystems(left: &Path, right: &Path) {
    use std::os::unix::fs::MetadataExt;

    assert_ne!(
        std::fs::metadata(left).unwrap().dev(),
        std::fs::metadata(right).unwrap().dev(),
        "expected {} and {} to be on different filesystems",
        left.display(),
        right.display()
    );
}

fn row_count(database: &Connection, table: &str) -> i64 {
    let sql = match table {
        "rift" => "SELECT COUNT(*) FROM rift",
        "trash" => "SELECT COUNT(*) FROM trash",
        _ => unreachable!("unexpected table: {table}"),
    };
    database.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn command_output(output: Output) -> CommandOutput {
    CommandOutput {
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn command_line(args: &[OsString]) -> String {
    std::iter::once(OsStr::new("rift"))
        .chain(args.iter().map(OsString::as_os_str))
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
