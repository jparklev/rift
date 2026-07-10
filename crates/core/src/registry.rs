use crate::{Error, Result, id::RiftId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct Record {
    pub(crate) id: RiftId,
    pub(crate) parent_id: Option<RiftId>,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathRecord {
    pub(crate) id: RiftId,
    pub(crate) path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MovedRecord {
    pub(crate) id: RiftId,
    pub(crate) original_path: PathBuf,
    pub(crate) trash_path: PathBuf,
}

/// A removal whose filesystem moves may have happened without the final
/// registry transfer. The journal is deliberately independent of `rift` rows:
/// it must survive long enough to finish a root unregistration that ultimately
/// deletes those rows by cascade.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingRemoval {
    pub(crate) id: RiftId,
    pub(crate) root_id: RiftId,
    pub(crate) root_path: PathBuf,
    pub(crate) unregister_root: bool,
    pub(crate) moved: Vec<MovedRecord>,
}

#[derive(Clone, Copy)]
pub(crate) enum SubtreeScope {
    IncludingRoot,
    DescendantsOnly,
}

impl SubtreeScope {
    fn min_depth(self) -> u8 {
        match self {
            Self::IncludingRoot => 0,
            Self::DescendantsOnly => 1,
        }
    }
}

pub(crate) struct Registry {
    database: Connection,
}

impl Registry {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database = Connection::open(path)?;
        database.execute_batch(
            "PRAGMA busy_timeout = 2000;
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
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
              );
              CREATE TABLE IF NOT EXISTS removal_operation (
                id TEXT PRIMARY KEY,
                root_id TEXT NOT NULL,
                root_path TEXT NOT NULL,
                unregister_root INTEGER NOT NULL,
                created_at INTEGER NOT NULL
              );
              CREATE INDEX IF NOT EXISTS removal_operation_root_idx
                ON removal_operation(root_id);
              CREATE TABLE IF NOT EXISTS removal_move (
                operation_id TEXT NOT NULL REFERENCES removal_operation(id) ON DELETE CASCADE,
                rift_id TEXT NOT NULL,
                original_path TEXT NOT NULL UNIQUE,
                trash_path TEXT NOT NULL UNIQUE,
                position INTEGER NOT NULL,
                PRIMARY KEY (operation_id, rift_id)
              );",
        )?;
        Ok(Self { database })
    }

    pub(crate) fn insert_root(&self, id: &RiftId, path: &Path) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, NULL, ?2, ?3)",
            params![id.as_str(), path_text(path)?, timestamp()],
        )?;
        Ok(())
    }

    pub(crate) fn insert_child(&self, id: &RiftId, parent_id: &RiftId, path: &Path) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                id.as_str(),
                parent_id.as_str(),
                path_text(path)?,
                timestamp()
            ],
        )?;
        Ok(())
    }

    pub(crate) fn record_at(&self, path: &Path) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE path = ?1",
                [path_text(path)?],
                record_from_row,
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn record_id(&self, id: &RiftId) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE id = ?1",
                [id.as_str()],
                record_from_row,
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn subtree(&self, id: &RiftId, scope: SubtreeScope) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare(
            "WITH RECURSIVE subtree(id, path, depth) AS (
               SELECT id, path, 0 FROM rift WHERE id = ?1
               UNION ALL
               SELECT rift.id, rift.path, subtree.depth + 1
               FROM rift JOIN subtree ON rift.parent_id = subtree.id
             ) SELECT id, path FROM subtree WHERE depth >= ?2 ORDER BY depth DESC, id",
        )?;
        let rows = statement
            .query_map(params![id.as_str(), scope.min_depth()], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn child_paths(&self, parent_id: &RiftId) -> Result<Vec<PathBuf>> {
        let mut statement = self
            .database
            .prepare("SELECT path FROM rift WHERE parent_id = ?1 ORDER BY created_at, id")?;
        Ok(statement
            .query_map([parent_id.as_str()], |row| {
                Ok(PathBuf::from(row.get::<_, String>(0)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    #[cfg(test)]
    pub(crate) fn trash_moved(&mut self, moved: &[MovedRecord]) -> Result<()> {
        let transaction = self.database.transaction()?;
        moved.iter().try_for_each(|record| -> Result<()> {
            transaction.execute(
                "INSERT INTO trash (id, path, removed_at) VALUES (?1, ?2, ?3)",
                params![
                    record.id.as_str(),
                    path_text(&record.trash_path)?,
                    timestamp()
                ],
            )?;
            transaction.execute("DELETE FROM rift WHERE id = ?1", [record.id.as_str()])?;
            Ok(())
        })?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist the full removal intent before a directory is renamed. A later
    /// manager open can then finish the move if this process exits between the
    /// filesystem phase and the active-to-trash registry transfer.
    pub(crate) fn stage_removal(
        &mut self,
        root: &Record,
        unregister_root: bool,
        moved: &[MovedRecord],
    ) -> Result<PendingRemoval> {
        let operation = PendingRemoval {
            id: RiftId::new(),
            root_id: root.id.clone(),
            root_path: root.path.clone(),
            unregister_root,
            moved: moved.to_vec(),
        };
        let transaction = self.database.transaction()?;
        transaction.execute(
            "INSERT INTO removal_operation (id, root_id, root_path, unregister_root, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                operation.id.as_str(),
                operation.root_id.as_str(),
                path_text(&operation.root_path)?,
                i64::from(operation.unregister_root),
                timestamp(),
            ],
        )?;
        operation
            .moved
            .iter()
            .enumerate()
            .try_for_each(|(position, record)| -> Result<()> {
                transaction.execute(
                    "INSERT INTO removal_move
                     (operation_id, rift_id, original_path, trash_path, position)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        operation.id.as_str(),
                        record.id.as_str(),
                        path_text(&record.original_path)?,
                        path_text(&record.trash_path)?,
                        position as i64,
                    ],
                )?;
                Ok(())
            })?;
        transaction.commit()?;
        Ok(operation)
    }

    pub(crate) fn pending_removals(&self) -> Result<Vec<PendingRemoval>> {
        let mut statement = self.database.prepare(
            "SELECT id, root_id, root_path, unregister_root
             FROM removal_operation ORDER BY created_at, id",
        )?;
        let operations = statement
            .query_map([], |row| {
                Ok((
                    RiftId::from_stored(row.get(0)?),
                    RiftId::from_stored(row.get(1)?),
                    PathBuf::from(row.get::<_, String>(2)?),
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        operations
            .into_iter()
            .map(|(id, root_id, root_path, unregister_root)| {
                self.load_pending_removal(id, root_id, root_path, unregister_root)
            })
            .collect()
    }

    pub(crate) fn pending_removals_for_root(
        &self,
        root_id: &RiftId,
    ) -> Result<Vec<PendingRemoval>> {
        let mut statement = self.database.prepare(
            "SELECT id, root_id, root_path, unregister_root
             FROM removal_operation WHERE root_id = ?1 ORDER BY created_at, id",
        )?;
        let operations = statement
            .query_map([root_id.as_str()], |row| {
                Ok((
                    RiftId::from_stored(row.get(0)?),
                    RiftId::from_stored(row.get(1)?),
                    PathBuf::from(row.get::<_, String>(2)?),
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        operations
            .into_iter()
            .map(|(id, root_id, root_path, unregister_root)| {
                self.load_pending_removal(id, root_id, root_path, unregister_root)
            })
            .collect()
    }

    pub(crate) fn pending_removal(&self, id: &RiftId) -> Result<Option<PendingRemoval>> {
        let operation = self
            .database
            .query_row(
                "SELECT id, root_id, root_path, unregister_root
                 FROM removal_operation WHERE id = ?1",
                [id.as_str()],
                |row| {
                    Ok((
                        RiftId::from_stored(row.get(0)?),
                        RiftId::from_stored(row.get(1)?),
                        PathBuf::from(row.get::<_, String>(2)?),
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()?;
        operation
            .map(|(id, root_id, root_path, unregister_root)| {
                self.load_pending_removal(id, root_id, root_path, unregister_root)
            })
            .transpose()
    }

    fn load_pending_removal(
        &self,
        id: RiftId,
        root_id: RiftId,
        root_path: PathBuf,
        unregister_root: bool,
    ) -> Result<PendingRemoval> {
        let mut statement = self.database.prepare(
            "SELECT rift_id, original_path, trash_path
             FROM removal_move WHERE operation_id = ?1 ORDER BY position",
        )?;
        let moved = statement
            .query_map([id.as_str()], |row| {
                Ok(MovedRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    original_path: PathBuf::from(row.get::<_, String>(1)?),
                    trash_path: PathBuf::from(row.get::<_, String>(2)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(PendingRemoval {
            id,
            root_id,
            root_path,
            unregister_root,
            moved,
        })
    }

    /// Atomically promotes a fully moved operation into ordinary trash rows.
    pub(crate) fn complete_removal(&mut self, operation: &PendingRemoval) -> Result<()> {
        let transaction = self.database.transaction()?;
        operation
            .moved
            .iter()
            .try_for_each(|record| -> Result<()> {
                transaction.execute(
                    "INSERT INTO trash (id, path, removed_at) VALUES (?1, ?2, ?3)",
                    params![
                        record.id.as_str(),
                        path_text(&record.trash_path)?,
                        timestamp()
                    ],
                )?;
                Ok(())
            })?;
        if operation.unregister_root {
            transaction.execute(
                "DELETE FROM rift WHERE id = ?1",
                [operation.root_id.as_str()],
            )?;
        } else {
            operation
                .moved
                .iter()
                .try_for_each(|record| -> Result<()> {
                    transaction.execute("DELETE FROM rift WHERE id = ?1", [record.id.as_str()])?;
                    Ok(())
                })?;
        }
        transaction.execute(
            "DELETE FROM removal_operation WHERE id = ?1",
            [operation.id.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn trashed_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self
            .database
            .prepare("SELECT id, path FROM trash ORDER BY removed_at, id")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_trash(&self, id: &RiftId) -> Result<()> {
        self.database
            .execute("DELETE FROM trash WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn active_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare("SELECT id, path FROM rift")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: RiftId::from_stored(row.get(0)?),
        parent_id: row.get::<_, Option<String>>(1)?.map(RiftId::from_stored),
        path: PathBuf::from(row.get::<_, String>(2)?),
    })
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
    use tempfile::TempDir;

    fn registry() -> (TempDir, Registry) {
        let temp = TempDir::new().unwrap();
        let registry = Registry::open(temp.path().join("registry.sqlite")).unwrap();
        (temp, registry)
    }

    #[test]
    fn uses_wal_and_busy_timeout() {
        let (_temp, registry) = registry();
        let journal_mode: String = registry
            .database
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let busy_timeout: i32 = registry
            .database
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 2000);
    }

    #[test]
    fn subtree_returns_descendants_before_ancestors() {
        let (temp, registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let sibling = temp.path().join("sibling");
        let grandchild = temp.path().join("grandchild");
        let root_id = id("root");
        let child_id = id("child");
        let sibling_id = id("sibling");
        let grandchild_id = id("grandchild");
        registry.insert_root(&root_id, &root).unwrap();
        registry.insert_child(&child_id, &root_id, &child).unwrap();
        registry
            .insert_child(&sibling_id, &root_id, &sibling)
            .unwrap();
        registry
            .insert_child(&grandchild_id, &child_id, &grandchild)
            .unwrap();

        let subtree = registry
            .subtree(&root_id, SubtreeScope::IncludingRoot)
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();
        let descendants = registry
            .subtree(&root_id, SubtreeScope::DescendantsOnly)
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();

        assert_eq!(subtree, vec!["grandchild", "child", "sibling", "root"]);
        assert_eq!(descendants, vec!["grandchild", "child", "sibling"]);
        assert_eq!(
            registry.child_paths(&root_id).unwrap(),
            vec![child, sibling]
        );
        assert_eq!(registry.active_paths().unwrap().len(), 4);
    }

    #[test]
    fn trash_moved_transfers_records_from_active_tree_to_trash() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let trash = temp.path().join(".trash/child");
        let root_id = id("root");
        let child_id = id("child");
        registry.insert_root(&root_id, &root).unwrap();
        registry.insert_child(&child_id, &root_id, &child).unwrap();

        registry
            .trash_moved(&[MovedRecord {
                id: child_id.clone(),
                original_path: child.clone(),
                trash_path: trash.clone(),
            }])
            .unwrap();

        assert!(registry.record_id(&root_id).unwrap().is_some());
        assert!(registry.record_id(&child_id).unwrap().is_none());
        assert_eq!(
            registry.trashed_paths().unwrap(),
            vec![PathRecord {
                id: child_id,
                path: trash,
            }]
        );
    }

    fn id(value: &str) -> RiftId {
        RiftId::from_stored(value.to_owned())
    }
}
