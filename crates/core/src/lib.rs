mod copy;
mod git;

use copy::{CopyStrategy, CowStrategy};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use ulid::Ulid;

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
    #[error("workspace is not a btrfs subvolume: {0}; run `rift init {0}` first")]
    InitializationRequired(PathBuf),
    #[error("unsupported filesystem entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("unsafe Git source: {0}")]
    UnsafeGit(String),
    #[error("directory is not managed by rift: {0}")]
    NotManaged(PathBuf),
    #[error("rift marker does not match the registry at: {0}")]
    MarkerMismatch(PathBuf),
    #[error("rift marker belongs to an unknown registry entry at: {0}")]
    UnknownMarker(PathBuf),
    #[error("rift directory already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("cannot remove the original registered workspace: {0}")]
    CannotRemoveRoot(PathBuf),
    #[error("cannot remove subtree while a recorded rift path is missing: {0}")]
    MissingRift(PathBuf),
    #[error("cannot copy a workspace into itself: {0}")]
    InsideSource(PathBuf),
}

pub struct Create {
    pub from: PathBuf,
    pub name: Option<String>,
    pub into: Option<PathBuf>,
}

#[derive(Clone)]
struct Record {
    id: String,
    parent_id: Option<String>,
    path: PathBuf,
}

pub struct Manager {
    database: Connection,
    copier: Box<dyn CopyStrategy>,
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
        Self::with_copier(path, Box::new(CowStrategy))
    }

    fn with_copier(path: impl AsRef<Path>, copier: Box<dyn CopyStrategy>) -> Result<Self> {
        let database = Connection::open(path)?;
        database.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS rift (
               id TEXT PRIMARY KEY,
               parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
               path TEXT NOT NULL UNIQUE,
               created_at INTEGER NOT NULL
              );
              CREATE INDEX IF NOT EXISTS rift_parent_id_idx ON rift(parent_id);
              CREATE TABLE IF NOT EXISTS trash (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                removed_at INTEGER NOT NULL
              );",
        )?;
        Ok(Self { database, copier })
    }

    pub fn create(&mut self, input: Create) -> Result<PathBuf> {
        let from = existing_directory(&input.from)?;
        let git = git::check_source(&from)?;
        let (source, register_source) = self.source(&from)?;
        let root = self.root(&source)?;
        let id = Ulid::new().to_string();
        let destination_parent = match input.into {
            Some(path) => absolute_path(&path)?,
            None => default_storage(&root.path)?,
        };
        let name = destination_name(input.name)?;
        if destination_parent.join(&name).starts_with(&from) {
            return Err(Error::InsideSource(destination_parent.join(name)));
        }
        fs::create_dir_all(&destination_parent)?;
        let destination_parent = fs::canonicalize(destination_parent)?;
        let destination = destination_parent.join(name);
        if destination.starts_with(&from) {
            return Err(Error::InsideSource(destination));
        }
        if destination.exists() {
            return Err(Error::AlreadyExists(destination));
        }

        if let Err(error) = self.copier.copy_directory(&from, &destination) {
            if destination.exists() {
                let _ = self.copier.remove_directory(&destination);
            }
            return Err(error);
        }

        let result = (|| {
            write_marker(&destination, &id)?;
            if git {
                git::hide_marker(&destination)?;
                git::detach_destination(&destination)?;
            }
            if register_source {
                write_marker(&from, &source.id)?;
                self.database.execute(
                    "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, NULL, ?2, ?3)",
                    params![source.id, path_text(&from)?, timestamp()],
                )?;
            }
            if git {
                git::hide_marker(&from)?;
            }
            self.database.execute(
                "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![id, source.id, path_text(&destination)?, timestamp()],
            )?;
            Ok(destination.clone())
        })();
        if result.is_err() {
            let _ = self.copier.remove_directory(&destination);
        }
        result
    }

    pub fn init(&mut self, at: impl AsRef<Path>) -> Result<Option<PathBuf>> {
        let at = existing_directory(at.as_ref())?;
        let git = git::check_source(&at)?;
        if let Some(record) = self.record_at_optional(&at)? {
            verify_marker(&record)?;
            let backup = self.copier.initialize_directory(&at)?;
            if git {
                git::hide_marker(&at)?;
            }
            return Ok(backup);
        }
        if read_marker(&at)?.is_some() {
            return Err(Error::MarkerMismatch(at));
        }

        let backup = self.copier.initialize_directory(&at)?;
        let id = Ulid::new().to_string();
        let result = (|| {
            write_marker(&at, &id)?;
            if git {
                git::hide_marker(&at)?;
            }
            self.database.execute(
                "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, NULL, ?2, ?3)",
                params![id, path_text(&at)?, timestamp()],
            )?;
            Ok(backup.clone())
        })();
        if result.is_err() {
            let _ = fs::remove_file(marker(&at));
        }
        result
    }

    pub fn remove(&mut self, at: impl AsRef<Path>) -> Result<()> {
        let at = existing_directory(at.as_ref())?;
        let record = self.record_at(&at)?;
        if record.parent_id.is_none() {
            return Err(Error::CannotRemoveRoot(at));
        }
        verify_marker(&record)?;
        let rows = self.subtree(&record.id, true)?;
        self.trash_rows(&rows)?;
        Ok(())
    }

    pub fn remove_all(&mut self, at: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let at = existing_directory(at.as_ref())?;
        let record = self.record_at(&at)?;
        verify_marker(&record)?;
        let rows = self.subtree(&record.id, false)?;
        self.trash_rows(&rows)?;
        Ok(rows.into_iter().map(|(_, path)| path).collect())
    }

    fn subtree(&self, id: &str, include_root: bool) -> Result<Vec<(String, PathBuf)>> {
        let mut statement = self.database.prepare(
            "WITH RECURSIVE subtree(id, path, depth) AS (
               SELECT id, path, 0 FROM rift WHERE id = ?1
               UNION ALL
               SELECT rift.id, rift.path, subtree.depth + 1
               FROM rift JOIN subtree ON rift.parent_id = subtree.id
             ) SELECT id, path FROM subtree WHERE depth >= ?2 ORDER BY depth DESC, id",
        )?;
        let rows = statement
            .query_map(params![id, if include_root { 0 } else { 1 }], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn trash_rows(&mut self, rows: &[(String, PathBuf)]) -> Result<()> {
        for (id, path) in rows {
            if !path.exists() {
                return Err(Error::MissingRift(path.clone()));
            }
            verify_marker(&Record {
                id: id.clone(),
                parent_id: None,
                path: path.clone(),
            })?;
        }
        let targets = rows
            .iter()
            .map(|(id, path)| Ok((id, path, trash_path(id, path)?)))
            .collect::<Result<Vec<_>>>()?;
        for (_, _, trash) in &targets {
            if trash.exists() {
                return Err(Error::AlreadyExists(trash.clone()));
            }
        }
        let mut moved = Vec::with_capacity(rows.len());
        for (id, path, trash) in targets {
            fs::create_dir_all(trash.parent().unwrap())?;
            if let Err(error) = fs::rename(path, &trash) {
                for (_, original, trashed) in moved.iter().rev() {
                    let _ = fs::rename(trashed, original);
                }
                return Err(error.into());
            }
            moved.push((id, path, trash));
        }
        let result = (|| {
            let transaction = self.database.transaction()?;
            for (id, _, path) in &moved {
                transaction.execute(
                    "INSERT INTO trash (id, path, removed_at) VALUES (?1, ?2, ?3)",
                    params![id, path_text(path)?, timestamp()],
                )?;
                transaction.execute("DELETE FROM rift WHERE id = ?1", [id])?;
            }
            transaction.commit()?;
            Ok(())
        })();
        if result.is_err() {
            for (_, original, trashed) in moved.iter().rev() {
                let _ = fs::rename(trashed, original);
            }
        }
        result
    }

    pub fn list(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.record_at(&existing_directory(of.as_ref())?)?;
        let mut statement = self
            .database
            .prepare("SELECT path FROM rift WHERE parent_id = ?1 ORDER BY created_at, id")?;
        Ok(statement
            .query_map([record.id], |row| {
                Ok(PathBuf::from(row.get::<_, String>(0)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn ancestors(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.record_at(&existing_directory(of.as_ref())?)?;
        let mut paths = Vec::new();
        let mut parent_id = record.parent_id;
        while let Some(id) = parent_id {
            let parent = self
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
            paths.push(parent.path);
            parent_id = parent.parent_id;
        }
        Ok(paths)
    }

    pub fn gc(&mut self) -> Result<Vec<PathBuf>> {
        let mut statement = self
            .database
            .prepare("SELECT id, path FROM trash ORDER BY removed_at, id")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        let mut removed = Vec::new();
        for (id, path) in rows {
            if path.exists() {
                self.copier.remove_directory(&path)?;
            }
            self.database
                .execute("DELETE FROM trash WHERE id = ?1", [&id])?;
            removed.push(path);
        }
        Ok(removed)
    }

    fn source(&self, path: &Path) -> Result<(Record, bool)> {
        if let Some(id) = read_marker(path)? {
            let record = self
                .record_id(&id)?
                .ok_or_else(|| Error::UnknownMarker(path.to_path_buf()))?;
            if record.path != path {
                return Err(Error::MarkerMismatch(path.to_path_buf()));
            }
            return Ok((record, false));
        }
        if self.record_at_optional(path)?.is_some() {
            return Err(Error::MarkerMismatch(path.to_path_buf()));
        }
        let id = Ulid::new().to_string();
        Ok((
            Record {
                id,
                parent_id: None,
                path: path.to_path_buf(),
            },
            true,
        ))
    }

    fn root(&self, record: &Record) -> Result<Record> {
        let mut current = record.clone();
        while let Some(id) = current.parent_id.clone() {
            current = self
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
        }
        Ok(current)
    }

    fn record_at(&self, path: &Path) -> Result<Record> {
        self.record_at_optional(path)?
            .ok_or_else(|| Error::NotManaged(path.to_path_buf()))
    }

    fn record_at_optional(&self, path: &Path) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE path = ?1",
                [path_text(path)?],
                |row| {
                    Ok(Record {
                        id: row.get(0)?,
                        parent_id: row.get(1)?,
                        path: PathBuf::from(row.get::<_, String>(2)?),
                    })
                },
            )
            .optional()
            .map_err(Error::from)
    }

    fn record_id(&self, id: &str) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE id = ?1",
                [id],
                |row| {
                    Ok(Record {
                        id: row.get(0)?,
                        parent_id: row.get(1)?,
                        path: PathBuf::from(row.get::<_, String>(2)?),
                    })
                },
            )
            .optional()
            .map_err(Error::from)
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

fn default_storage(root: &Path) -> Result<PathBuf> {
    let parent = root
        .parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", root.display())))?;
    let name = root
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", root.display())))?;
    Ok(parent.join(".rifts").join(name))
}

fn trash_path(id: &str, path: &Path) -> Result<PathBuf> {
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

fn destination_name(name: Option<String>) -> Result<String> {
    let name = name.unwrap_or_else(generated_name);
    if name.is_empty() || name == "." || name == ".." || Path::new(&name).components().count() != 1
    {
        return Err(Error::Path(format!("invalid rift name: {name}")));
    }
    Ok(name)
}

fn generated_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "amber", "bold", "brisk", "calm", "cedar", "clear", "cobalt", "coral", "dawn", "ember",
        "gentle", "golden", "jade", "lively", "lunar", "mellow", "misty", "noble", "quiet",
        "rapid", "river", "silver", "solar", "spruce", "steady", "swift", "tidal", "verdant",
        "violet", "warm", "wild", "winter",
    ];
    const NOUNS: &[&str] = &[
        "badger", "brook", "canyon", "cedar", "comet", "dune", "falcon", "field", "forest",
        "harbor", "heron", "island", "lantern", "maple", "meadow", "mesa", "otter", "peak", "pine",
        "reef", "ridge", "robin", "sparrow", "summit", "thicket", "trail", "valley", "willow",
        "wren", "yarrow", "zephyr", "fox",
    ];

    let mut rng = rand::rng();
    format!(
        "{}-{}",
        ADJECTIVES[rng.random_range(0..ADJECTIVES.len())],
        NOUNS[rng.random_range(0..NOUNS.len())]
    )
}

fn marker(path: &Path) -> PathBuf {
    path.join(".rift")
}

fn write_marker(path: &Path, id: &str) -> Result<()> {
    fs::write(marker(path), format!("{id}\n"))?;
    Ok(())
}

fn read_marker(path: &Path) -> Result<Option<String>> {
    let marker = marker(path);
    if !marker.exists() {
        return Ok(None);
    }
    Ok(Some(fs::read_to_string(marker)?.trim().to_owned()))
}

fn verify_marker(record: &Record) -> Result<()> {
    if read_marker(&record.path)?.as_deref() == Some(&record.id) {
        return Ok(());
    }
    Err(Error::MarkerMismatch(record.path.clone()))
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::Path(format!("path is not valid UTF-8: {}", path.display())))
}

fn timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::{FailureStrategy, TestStrategy};
    use std::process::Command;
    use tempfile::TempDir;

    fn manager(temp: &TempDir) -> Manager {
        Manager::with_copier(temp.path().join("registry.sqlite"), Box::new(TestStrategy)).unwrap()
    }

    fn source(temp: &TempDir) -> PathBuf {
        let source = temp.path().join("app");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "hello").unwrap();
        source
    }

    #[test]
    fn create_tracks_parentage_and_default_storage() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();

        assert_eq!(first, temp.path().join(".rifts/app/first"));
        assert_eq!(second, temp.path().join(".rifts/app/second"));
        assert_ne!(
            fs::read_to_string(source.join(".rift")).unwrap(),
            fs::read_to_string(first.join(".rift")).unwrap()
        );
        assert_eq!(manager.list(&source).unwrap(), vec![first.clone()]);
        assert_eq!(manager.ancestors(&second).unwrap(), vec![first, source]);
    }

    #[test]
    fn init_registers_a_root_workspace_without_creating_a_child() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);

        assert!(manager.init(&source).unwrap().is_none());
        assert!(source.join(".rift").exists());
        assert!(manager.list(&source).unwrap().is_empty());
        assert!(manager.init(&source).unwrap().is_none());
    }

    #[test]
    fn create_generates_readable_names_independent_of_ulid_identity() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);

        let destination = manager
            .create(Create {
                from: source,
                name: None,
                into: None,
            })
            .unwrap();
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
    fn remove_trashes_a_full_subtree_and_gc_deletes_it() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();

        let first_trash = trash_path(
            &fs::read_to_string(first.join(".rift"))
                .unwrap()
                .trim()
                .to_owned(),
            &first,
        )
        .unwrap();
        let second_trash = trash_path(
            &fs::read_to_string(second.join(".rift"))
                .unwrap()
                .trim()
                .to_owned(),
            &second,
        )
        .unwrap();

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
        assert!(matches!(
            manager.remove(&source),
            Err(Error::CannotRemoveRoot(_))
        ));
    }

    #[test]
    fn remove_all_deletes_descendants_and_preserves_the_selected_workspace() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        let sibling = manager
            .create(Create {
                from: source.clone(),
                name: Some("sibling".into()),
                into: None,
            })
            .unwrap();
        let first_id = fs::read_to_string(first.join(".rift")).unwrap();
        let first_trash = trash_path(first_id.trim(), &first).unwrap();

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
        let first = manager
            .create(Create {
                from: source,
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
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
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
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
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        let first_id = fs::read_to_string(first.join(".rift")).unwrap();
        let second_id = fs::read_to_string(second.join(".rift")).unwrap();
        let first_trash = trash_path(first_id.trim(), &first).unwrap();
        let second_trash = trash_path(second_id.trim(), &second).unwrap();
        manager.remove(&first).unwrap();

        let deleted = manager.gc().unwrap();
        assert!(deleted.contains(&second_trash));
        assert!(deleted.contains(&first_trash));
        assert_eq!(deleted.len(), 2);
        assert!(manager.list(&source).unwrap().is_empty());
    }

    #[test]
    fn gc_has_no_effect_on_active_rifts() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        assert!(manager.gc().unwrap().is_empty());
        assert_eq!(manager.ancestors(&second).unwrap(), vec![first, source]);
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

        let destination = manager
            .create(Create {
                from: source.clone(),
                name: Some("git".into()),
                into: None,
            })
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
    fn unsafe_git_source_is_rejected_without_registering_it() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        run(&source, &["init"]);
        fs::write(source.join(".git/MERGE_HEAD"), "commit").unwrap();
        let mut manager = manager(&temp);

        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("unsafe".into()),
                into: None,
            }),
            Err(Error::UnsafeGit(_))
        ));
        assert!(!source.join(".rift").exists());
    }

    #[test]
    fn unavailable_cow_does_not_register_the_source() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = Manager::with_copier(
            temp.path().join("registry.sqlite"),
            Box::new(FailureStrategy),
        )
        .unwrap();

        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("failure".into()),
                into: None,
            }),
            Err(Error::CowUnavailable(_))
        ));
        assert!(!source.join(".rift").exists());
        assert!(manager.record_at_optional(&source).unwrap().is_none());
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
}
