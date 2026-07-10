use crate::{Error, Result, id::RiftId};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use std::path::{Path, PathBuf};

const CURRENT_SCHEMA: &str = "
  CREATE TABLE IF NOT EXISTS rift (
    id BLOB PRIMARY KEY,
    parent_id BLOB REFERENCES rift(id) ON DELETE CASCADE,
    path TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS rift_parent_created_idx
    ON rift(parent_id, created_at, id);
  -- The ordered composite index also covers parent_id lookups.
  -- Drop the legacy prefix index during open so existing registries do not
  -- retain redundant per-workspace metadata.
  DROP INDEX IF EXISTS rift_parent_id_idx;
  CREATE TABLE IF NOT EXISTS trash (
    id BLOB PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    removed_at INTEGER NOT NULL
  );
  CREATE TABLE IF NOT EXISTS removal_operation (
    id BLOB PRIMARY KEY,
    root_id BLOB NOT NULL,
    root_path TEXT NOT NULL,
    unregister_root INTEGER NOT NULL,
    created_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS removal_operation_root_idx
    ON removal_operation(root_id);
  CREATE TABLE IF NOT EXISTS removal_move (
    operation_id BLOB NOT NULL REFERENCES removal_operation(id) ON DELETE CASCADE,
    rift_id BLOB NOT NULL,
    original_path TEXT NOT NULL UNIQUE,
    trash_path TEXT NOT NULL UNIQUE,
    position INTEGER NOT NULL,
    PRIMARY KEY (operation_id, rift_id)
  );";

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
        let mut database = Connection::open(path)?;
        database.execute_batch(
            "PRAGMA busy_timeout = 2000;
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;",
        )?;
        migrate_text_registry_ids(&mut database)?;
        database.execute_batch(CURRENT_SCHEMA)?;
        Ok(Self { database })
    }

    pub(crate) fn insert_root(&self, id: &RiftId, path: &Path) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, NULL, ?2, ?3)",
            params![id, path_text(path)?, timestamp()],
        )?;
        Ok(())
    }

    pub(crate) fn insert_child(&self, id: &RiftId, parent_id: &RiftId, path: &Path) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, parent_id, path_text(path)?, timestamp()],
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
        // Marker text is user-controlled. Preserve the existing behavior for
        // a malformed or differently cased marker: it simply cannot name a
        // registry row and is reported by the manager as an unknown marker.
        if id.database_bytes().is_err() {
            return Ok(None);
        }
        self.database
            .query_row(
                "SELECT id, parent_id, path FROM rift WHERE id = ?1",
                [id],
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
            .query_map(params![id, scope.min_depth()], |row| {
                Ok(PathRecord {
                    id: row.get(0)?,
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
            .query_map([parent_id], |row| {
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
                params![record.id, path_text(&record.trash_path)?, timestamp()],
            )?;
            transaction.execute("DELETE FROM rift WHERE id = ?1", [&record.id])?;
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
                operation.id,
                operation.root_id,
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
                        operation.id,
                        record.id,
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
                    row.get(0)?,
                    row.get(1)?,
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
            .query_map([root_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
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
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
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
            .query_map([&id], |row| {
                Ok(MovedRecord {
                    id: row.get(0)?,
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
                    params![record.id, path_text(&record.trash_path)?, timestamp()],
                )?;
                Ok(())
            })?;
        if operation.unregister_root {
            transaction.execute("DELETE FROM rift WHERE id = ?1", [&operation.root_id])?;
        } else {
            operation
                .moved
                .iter()
                .try_for_each(|record| -> Result<()> {
                    transaction.execute("DELETE FROM rift WHERE id = ?1", [&record.id])?;
                    Ok(())
                })?;
        }
        transaction.execute(
            "DELETE FROM removal_operation WHERE id = ?1",
            [&operation.id],
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
                    id: row.get(0)?,
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_trash(&self, id: &RiftId) -> Result<()> {
        self.database
            .execute("DELETE FROM trash WHERE id = ?1", [id])?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn active_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare("SELECT id, path FROM rift")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: row.get(0)?,
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

/// Upgrade the original text-ULID schema without losing active workspaces,
/// trash entries, or an in-flight removal journal. SQLite cannot alter a
/// column's storage class in place, so the upgrade atomically rebuilds each
/// affected table and keeps foreign-key enforcement disabled only during that
/// transaction.
fn migrate_text_registry_ids(database: &mut Connection) -> Result<()> {
    if !table_column_is_text(database, "rift", "id")? {
        return Ok(());
    }

    database.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration = (|| -> Result<bool> {
        // An immediate transaction serializes concurrent first opens. Recheck
        // under that lock because another process may have upgraded the
        // registry after this function's inexpensive initial schema probe.
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !table_column_is_text(&transaction, "rift", "id")? {
            transaction.commit()?;
            return Ok(false);
        }
        migrate_rift_ids(&transaction)?;
        migrate_trash_ids(&transaction)?;
        migrate_removal_journal_ids(&transaction)?;
        if has_foreign_key_violation(&transaction)? {
            return Err(Error::Database(rusqlite::Error::InvalidQuery));
        }
        transaction.commit()?;
        Ok(true)
    })();
    let restore_foreign_keys = database.execute_batch("PRAGMA foreign_keys = ON;");
    let migrated = match migration {
        Ok(migrated) => {
            restore_foreign_keys?;
            migrated
        }
        Err(error) => {
            let _ = restore_foreign_keys;
            return Err(error);
        }
    };
    if migrated {
        // Rebuilding tables leaves the old text B-trees on SQLite's freelist.
        // Compact once so an existing registry realizes the same disk-space
        // reduction as a fresh BLOB-backed registry.
        database.execute_batch("VACUUM;")?;
    }
    Ok(())
}

fn table_column_is_text(database: &Connection, table: &str, column: &str) -> Result<bool> {
    let storage = database
        .query_row(
            "SELECT type FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(storage.is_some_and(|storage| storage.eq_ignore_ascii_case("TEXT")))
}

fn table_exists(transaction: &Transaction<'_>, table: &str) -> rusqlite::Result<bool> {
    transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
         )",
        [table],
        |row| row.get::<_, i64>(0).map(|exists| exists != 0),
    )
}

fn has_foreign_key_violation(transaction: &Transaction<'_>) -> rusqlite::Result<bool> {
    transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        [],
        |row| row.get::<_, i64>(0).map(|exists| exists != 0),
    )
}

fn migrate_rift_ids(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    let rows = {
        let mut statement =
            transaction.prepare("SELECT id, parent_id, path, created_at FROM rift")?;
        statement
            .query_map([], |row| {
                Ok((
                    RiftId::from_stored(row.get::<_, String>(0)?),
                    row.get::<_, Option<String>>(1)?.map(RiftId::from_stored),
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };

    transaction.execute_batch(
        "DROP INDEX IF EXISTS rift_parent_id_idx;
         DROP INDEX IF EXISTS rift_parent_created_idx;
         ALTER TABLE rift RENAME TO rift_text_legacy;
         CREATE TABLE rift (
           id BLOB PRIMARY KEY,
           parent_id BLOB REFERENCES rift(id) ON DELETE CASCADE,
           path TEXT NOT NULL UNIQUE,
           created_at INTEGER NOT NULL
         );",
    )?;
    for (id, parent_id, path, created_at) in rows {
        transaction.execute(
            "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, parent_id, path, created_at],
        )?;
    }
    transaction.execute_batch("DROP TABLE rift_text_legacy;")
}

fn migrate_trash_ids(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    if !table_exists(transaction, "trash")? {
        return Ok(());
    }
    let rows = {
        let mut statement = transaction.prepare("SELECT id, path, removed_at FROM trash")?;
        statement
            .query_map([], |row| {
                Ok((
                    RiftId::from_stored(row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };

    transaction.execute_batch(
        "ALTER TABLE trash RENAME TO trash_text_legacy;
         CREATE TABLE trash (
           id BLOB PRIMARY KEY,
           path TEXT NOT NULL UNIQUE,
           removed_at INTEGER NOT NULL
         );",
    )?;
    for (id, path, removed_at) in rows {
        transaction.execute(
            "INSERT INTO trash (id, path, removed_at) VALUES (?1, ?2, ?3)",
            params![id, path, removed_at],
        )?;
    }
    transaction.execute_batch("DROP TABLE trash_text_legacy;")
}

fn migrate_removal_journal_ids(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    if !table_exists(transaction, "removal_operation")? {
        return Ok(());
    }
    let operations = {
        let mut statement = transaction.prepare(
            "SELECT id, root_id, root_path, unregister_root, created_at FROM removal_operation",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    RiftId::from_stored(row.get::<_, String>(0)?),
                    RiftId::from_stored(row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let moves = if table_exists(transaction, "removal_move")? {
        let mut statement = transaction.prepare(
            "SELECT operation_id, rift_id, original_path, trash_path, position FROM removal_move",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    RiftId::from_stored(row.get::<_, String>(0)?),
                    RiftId::from_stored(row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let had_moves = table_exists(transaction, "removal_move")?;

    transaction.execute_batch("DROP INDEX IF EXISTS removal_operation_root_idx;")?;
    if had_moves {
        transaction
            .execute_batch("ALTER TABLE removal_move RENAME TO removal_move_text_legacy;")?;
    }
    transaction.execute_batch(
        "ALTER TABLE removal_operation RENAME TO removal_operation_text_legacy;
         CREATE TABLE removal_operation (
           id BLOB PRIMARY KEY,
           root_id BLOB NOT NULL,
           root_path TEXT NOT NULL,
           unregister_root INTEGER NOT NULL,
           created_at INTEGER NOT NULL
         );",
    )?;
    for (id, root_id, root_path, unregister_root, created_at) in operations {
        transaction.execute(
            "INSERT INTO removal_operation (id, root_id, root_path, unregister_root, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, root_id, root_path, unregister_root, created_at],
        )?;
    }
    if had_moves {
        transaction.execute_batch(
            "CREATE TABLE removal_move (
               operation_id BLOB NOT NULL REFERENCES removal_operation(id) ON DELETE CASCADE,
               rift_id BLOB NOT NULL,
               original_path TEXT NOT NULL UNIQUE,
               trash_path TEXT NOT NULL UNIQUE,
               position INTEGER NOT NULL,
               PRIMARY KEY (operation_id, rift_id)
             );",
        )?;
        for (operation_id, rift_id, original_path, trash_path, position) in moves {
            transaction.execute(
                "INSERT INTO removal_move (operation_id, rift_id, original_path, trash_path, position)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![operation_id, rift_id, original_path, trash_path, position],
            )?;
        }
        transaction.execute_batch("DROP TABLE removal_move_text_legacy;")?;
    }
    transaction.execute_batch("DROP TABLE removal_operation_text_legacy;")
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: row.get(0)?,
        parent_id: row.get(1)?,
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
    fn upgrades_the_legacy_parent_index_without_retaining_a_duplicate() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE rift (
                   id TEXT PRIMARY KEY,
                   parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
                   path TEXT NOT NULL UNIQUE,
                   created_at INTEGER NOT NULL
                 );
                 CREATE INDEX rift_parent_id_idx ON rift(parent_id);",
            )
            .unwrap();
        drop(legacy);

        let registry = Registry::open(&path).unwrap();
        let indexes = registry
            .database
            .prepare("PRAGMA index_list('rift')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert!(indexes.contains(&"rift_parent_created_idx".to_owned()));
        assert!(!indexes.contains(&"rift_parent_id_idx".to_owned()));
    }

    #[test]
    fn migrates_text_ids_without_losing_active_trash_or_pending_removal_rows() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.sqlite");
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let trash = temp.path().join(".trash/child");
        let root_id = id("root");
        let child_id = id("child");
        let operation_id = id("operation");
        let trashed_id = id("trashed");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE rift (
                   id TEXT PRIMARY KEY,
                   parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
                   path TEXT NOT NULL UNIQUE,
                   created_at INTEGER NOT NULL
                 );
                 CREATE INDEX rift_parent_id_idx ON rift(parent_id);
                 CREATE TABLE trash (
                   id TEXT PRIMARY KEY,
                   path TEXT NOT NULL UNIQUE,
                   removed_at INTEGER NOT NULL
                 );
                 CREATE TABLE removal_operation (
                   id TEXT PRIMARY KEY,
                   root_id TEXT NOT NULL,
                   root_path TEXT NOT NULL,
                   unregister_root INTEGER NOT NULL,
                   created_at INTEGER NOT NULL
                 );
                 CREATE INDEX removal_operation_root_idx ON removal_operation(root_id);
                 CREATE TABLE removal_move (
                   operation_id TEXT NOT NULL REFERENCES removal_operation(id) ON DELETE CASCADE,
                   rift_id TEXT NOT NULL,
                   original_path TEXT NOT NULL UNIQUE,
                   trash_path TEXT NOT NULL UNIQUE,
                   position INTEGER NOT NULL,
                   PRIMARY KEY (operation_id, rift_id)
                 );",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, NULL, ?2, 1)",
                params![root_id.as_str(), path_text(&root).unwrap()],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO rift (id, parent_id, path, created_at) VALUES (?1, ?2, ?3, 2)",
                params![
                    child_id.as_str(),
                    root_id.as_str(),
                    path_text(&child).unwrap()
                ],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO trash (id, path, removed_at) VALUES (?1, ?2, 3)",
                params![trashed_id.as_str(), path_text(&trash).unwrap()],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO removal_operation (id, root_id, root_path, unregister_root, created_at)
                 VALUES (?1, ?2, ?3, 0, 4)",
                params![operation_id.as_str(), root_id.as_str(), path_text(&root).unwrap()],
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO removal_move (operation_id, rift_id, original_path, trash_path, position)
                 VALUES (?1, ?2, ?3, ?4, 0)",
                params![
                    operation_id.as_str(),
                    child_id.as_str(),
                    path_text(&child).unwrap(),
                    path_text(&trash).unwrap(),
                ],
            )
            .unwrap();
        drop(legacy);

        let registry = Registry::open(&path).unwrap();
        assert_eq!(registry.child_paths(&root_id).unwrap(), vec![child.clone()]);
        assert_eq!(
            registry.record_id(&child_id).unwrap().unwrap().parent_id,
            Some(root_id.clone())
        );
        assert_eq!(
            registry.trashed_paths().unwrap(),
            vec![PathRecord {
                id: trashed_id,
                path: trash.clone(),
            }]
        );
        assert_eq!(
            registry
                .pending_removal(&operation_id)
                .unwrap()
                .unwrap()
                .moved,
            vec![MovedRecord {
                id: child_id,
                original_path: child,
                trash_path: trash,
            }]
        );
        for (table, column) in [
            ("rift", "id"),
            ("rift", "parent_id"),
            ("trash", "id"),
            ("removal_operation", "id"),
            ("removal_operation", "root_id"),
            ("removal_move", "operation_id"),
            ("removal_move", "rift_id"),
        ] {
            let (kind, length): (String, i64) = registry
                .database
                .query_row(
                    &format!(
                        "SELECT typeof({column}), length({column}) FROM {table} \
                         WHERE {column} IS NOT NULL LIMIT 1"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((kind.as_str(), length), ("blob", 16), "{table}.{column}");
        }
        let violations: i64 = registry
            .database
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        let freelist: i64 = registry
            .database
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            freelist, 0,
            "migration should reclaim the retired text B-trees"
        );
        drop(registry);

        let reopened = Registry::open(&path).unwrap();
        assert!(reopened.record_id(&root_id).unwrap().is_some());
    }

    #[test]
    fn rejects_invalid_legacy_ids_without_partially_rebuilding_the_registry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE rift (
                   id TEXT PRIMARY KEY,
                   parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
                   path TEXT NOT NULL UNIQUE,
                   created_at INTEGER NOT NULL
                 );
                 INSERT INTO rift VALUES ('not-a-canonical-ulid', NULL, '/root', 0);",
            )
            .unwrap();
        drop(legacy);

        assert!(Registry::open(&path).is_err());

        let original = Connection::open(&path).unwrap();
        let type_: String = original
            .query_row(
                "SELECT type FROM pragma_table_info('rift') WHERE name = 'id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let count: i64 = original
            .query_row("SELECT count(*) FROM rift", [], |row| row.get(0))
            .unwrap();
        assert_eq!(type_, "TEXT");
        assert_eq!(count, 1);
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

        assert_eq!(
            subtree,
            vec![
                grandchild_id.to_string(),
                child_id.to_string(),
                sibling_id.to_string(),
                root_id.to_string(),
            ]
        );
        assert_eq!(
            descendants,
            vec![
                grandchild_id.to_string(),
                child_id.to_string(),
                sibling_id.to_string(),
            ]
        );
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
        let ulid = match value {
            "root" => "00000000000000000000000001",
            "child" => "00000000000000000000000002",
            "sibling" => "00000000000000000000000003",
            "grandchild" => "00000000000000000000000004",
            "operation" => "00000000000000000000000005",
            "trashed" => "00000000000000000000000006",
            _ => panic!("unknown registry test ID: {value}"),
        };
        RiftId::from_stored(ulid.to_owned())
    }
}
