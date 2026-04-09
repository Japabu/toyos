//! SQLite-based storage backend for the global cache tracker.

use super::*;
use crate::CargoResult;
use crate::util::interning::InternedString;
use crate::util::sqlite::{self, Migration, basic_migration};
use rusqlite::{Connection, ErrorCode, params};
use std::collections::{HashMap, hash_map};
use std::path::Path;
use tracing::trace;

/// The filename of the SQLite database.
const GLOBAL_CACHE_FILENAME: &str = ".global-cache";

/// Type for SQL columns that refer to the primary key of their parent table.
#[derive(Copy, Clone, Debug, PartialEq)]
struct ParentId(i64);

impl rusqlite::types::FromSql for ParentId {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let i = i64::column_result(value)?;
        Ok(ParentId(i))
    }
}

impl rusqlite::types::ToSql for ParentId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0))
    }
}

fn migrations() -> Vec<Migration> {
    vec![
        basic_migration(
            "CREATE TABLE registry_index (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                timestamp INTEGER NOT NULL
            )",
        ),
        basic_migration(
            "CREATE TABLE registry_crate (
                registry_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                size INTEGER NOT NULL,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (registry_id, name),
                FOREIGN KEY (registry_id) REFERENCES registry_index (id) ON DELETE CASCADE
             )",
        ),
        basic_migration(
            "CREATE TABLE registry_src (
                registry_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                size INTEGER,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (registry_id, name),
                FOREIGN KEY (registry_id) REFERENCES registry_index (id) ON DELETE CASCADE
             )",
        ),
        basic_migration(
            "CREATE TABLE git_db (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT UNIQUE NOT NULL,
                timestamp INTEGER NOT NULL
             )",
        ),
        basic_migration(
            "CREATE TABLE git_checkout (
                git_id INTEGER NOT NULL,
                name TEXT UNIQUE NOT NULL,
                size INTEGER,
                timestamp INTEGER NOT NULL,
                PRIMARY KEY (git_id, name),
                FOREIGN KEY (git_id) REFERENCES git_db (id) ON DELETE CASCADE
             )",
        ),
        basic_migration(
            "CREATE TABLE global_data (
                last_auto_gc INTEGER NOT NULL
            )",
        ),
        Box::new(|conn| {
            conn.execute(
                "INSERT INTO global_data (last_auto_gc) VALUES (?1)",
                [now()],
            )?;
            Ok(())
        }),
    ]
}

/// Upserts parent table entries, updating the key cache.
fn upsert_parent_entries(
    conn: &Connection,
    table_name: &str,
    timestamps: impl IntoIterator<Item = (InternedString, Timestamp)>,
    keys: &mut HashMap<InternedString, ParentId>,
) -> CargoResult<()> {
    let select_sql = format!("SELECT id, timestamp FROM {table_name} WHERE name = ?1");
    let insert_sql = format!(
        "INSERT INTO {table_name} (name, timestamp)
         VALUES (?1, ?2)
         ON CONFLICT DO UPDATE SET timestamp=excluded.timestamp
         RETURNING id"
    );
    let update_sql = format!("UPDATE {table_name} SET timestamp = ?1 WHERE id = ?2");
    let mut select_stmt = conn.prepare_cached(&select_sql)?;
    let mut insert_stmt = conn.prepare_cached(&insert_sql)?;
    let mut update_stmt = conn.prepare_cached(&update_sql)?;
    for (encoded_name, new_timestamp) in timestamps {
        trace!(target: "gc", "insert {table_name} {encoded_name:?} {new_timestamp}");
        let mut rows = select_stmt.query([&*encoded_name])?;
        let id = if let Some(row) = rows.next()? {
            let id: ParentId = row.get_unwrap(0);
            let timestamp: Timestamp = row.get_unwrap(1);
            if timestamp < new_timestamp - UPDATE_RESOLUTION {
                update_stmt.execute(params![new_timestamp, id])?;
            }
            id
        } else {
            insert_stmt.query_row(params![&*encoded_name, new_timestamp], |row| {
                row.get(0)
            })?
        };
        match keys.entry(encoded_name) {
            hash_map::Entry::Occupied(o) => {
                assert_eq!(*o.get(), id);
            }
            hash_map::Entry::Vacant(v) => {
                v.insert(id);
            }
        }
    }
    Ok(())
}

/// SQLite-based cache backend.
#[derive(Debug)]
pub(super) struct SqliteBackend {
    conn: Connection,
    /// Cache of registry keys for faster fetching.
    registry_keys: HashMap<InternedString, ParentId>,
    /// Cache of git keys for faster fetching.
    git_keys: HashMap<InternedString, ParentId>,
}

impl SqliteBackend {
    pub const FILENAME: &'static str = GLOBAL_CACHE_FILENAME;

    pub fn open(path: &Path) -> CargoResult<SqliteBackend> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        sqlite::migrate(&mut conn, &migrations())?;
        Ok(SqliteBackend {
            conn,
            registry_keys: HashMap::new(),
            git_keys: HashMap::new(),
        })
    }

    fn id_from_name(
        conn: &Connection,
        table_name: &str,
        encoded_name: &str,
    ) -> CargoResult<Option<ParentId>> {
        let mut stmt =
            conn.prepare_cached(&format!("SELECT id FROM {table_name} WHERE name = ?"))?;
        match stmt.query_row([encoded_name], |row| row.get(0)) {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn registry_id(&mut self, encoded_registry_name: InternedString) -> CargoResult<ParentId> {
        match self.registry_keys.get(&encoded_registry_name) {
            Some(i) => Ok(*i),
            None => {
                let Some(id) = Self::id_from_name(
                    &self.conn,
                    "registry_index",
                    &encoded_registry_name,
                )?
                else {
                    anyhow::bail!(
                        "expected registry_index {encoded_registry_name} to exist, but wasn't found"
                    );
                };
                self.registry_keys.insert(encoded_registry_name, id);
                Ok(id)
            }
        }
    }

    fn git_id(&mut self, encoded_git_name: InternedString) -> CargoResult<ParentId> {
        match self.git_keys.get(&encoded_git_name) {
            Some(i) => Ok(*i),
            None => {
                let Some(id) =
                    Self::id_from_name(&self.conn, "git_db", &encoded_git_name)?
                else {
                    anyhow::bail!("expected git_db {encoded_git_name} to exist, but wasn't found")
                };
                self.git_keys.insert(encoded_git_name, id);
                Ok(id)
            }
        }
    }

    // --- Query methods ---

    pub fn registry_index_all(&self) -> CargoResult<Vec<(RegistryIndex, Timestamp)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name, timestamp FROM registry_index")?;
        let rows = stmt
            .query_map([], |row| {
                let encoded_registry_name = row.get_unwrap(0);
                let timestamp = row.get_unwrap(1);
                Ok((RegistryIndex { encoded_registry_name }, timestamp))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn registry_crate_all(&self) -> CargoResult<Vec<(RegistryCrate, Timestamp)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT registry_index.name, registry_crate.name, registry_crate.size, registry_crate.timestamp
             FROM registry_index, registry_crate
             WHERE registry_crate.registry_id = registry_index.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    RegistryCrate {
                        encoded_registry_name: row.get_unwrap(0),
                        crate_filename: row.get_unwrap(1),
                        size: row.get_unwrap(2),
                    },
                    row.get_unwrap(3),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn registry_src_all(&self) -> CargoResult<Vec<(RegistrySrc, Timestamp)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT registry_index.name, registry_src.name, registry_src.size, registry_src.timestamp
             FROM registry_index, registry_src
             WHERE registry_src.registry_id = registry_index.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    RegistrySrc {
                        encoded_registry_name: row.get_unwrap(0),
                        package_dir: row.get_unwrap(1),
                        size: row.get_unwrap(2),
                    },
                    row.get_unwrap(3),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn git_db_all(&self) -> CargoResult<Vec<(GitDb, Timestamp)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT name, timestamp FROM git_db")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    GitDb { encoded_git_name: row.get_unwrap(0) },
                    row.get_unwrap(1),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn git_checkout_all(&self) -> CargoResult<Vec<(GitCheckout, Timestamp)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT git_db.name, git_checkout.name, git_checkout.size, git_checkout.timestamp
             FROM git_db, git_checkout
             WHERE git_checkout.git_id = git_db.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    GitCheckout {
                        encoded_git_name: row.get_unwrap(0),
                        short_name: row.get_unwrap(1),
                        size: row.get_unwrap(2),
                    },
                    row.get_unwrap(3),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // --- Metadata ---

    pub fn last_auto_gc(&self) -> CargoResult<Timestamp> {
        Ok(self.conn.query_row(
            "SELECT last_auto_gc FROM global_data",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn set_last_auto_gc(&mut self, timestamp: Timestamp) -> CargoResult<()> {
        self.conn
            .execute("UPDATE global_data SET last_auto_gc = ?1", [timestamp])?;
        Ok(())
    }

    // --- Insert if missing ---

    pub fn insert_registry_index_if_missing(
        &mut self,
        name: &str,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.conn.execute(
            "INSERT INTO registry_index (name, timestamp) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![name, timestamp],
        )?;
        Ok(())
    }

    pub fn insert_registry_crate_if_missing(
        &mut self,
        registry: &str,
        name: &str,
        size: u64,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "registry_index", registry)? else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO registry_crate (registry_id, name, size, timestamp)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            params![id, name, size, timestamp],
        )?;
        Ok(())
    }

    pub fn insert_registry_src_if_missing(
        &mut self,
        registry: &str,
        name: &str,
        size: Option<u64>,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "registry_index", registry)? else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO registry_src (registry_id, name, size, timestamp)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            params![id, name, size, timestamp],
        )?;
        Ok(())
    }

    pub fn insert_git_db_if_missing(
        &mut self,
        name: &str,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.conn.execute(
            "INSERT INTO git_db (name, timestamp) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
            params![name, timestamp],
        )?;
        Ok(())
    }

    pub fn insert_git_checkout_if_missing(
        &mut self,
        git_db: &str,
        name: &str,
        size: Option<u64>,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "git_db", git_db)? else {
            return Ok(());
        };
        self.conn.execute(
            "INSERT INTO git_checkout (git_id, name, size, timestamp)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            params![id, name, size, timestamp],
        )?;
        Ok(())
    }

    // --- Delete ---

    pub fn delete_registry_index(&mut self, name: &str) -> CargoResult<()> {
        self.conn
            .execute("DELETE FROM registry_index WHERE name = ?1", [name])?;
        Ok(())
    }

    pub fn delete_registry_crate(&mut self, registry: &str, name: &str) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "registry_index", registry)? else {
            return Ok(());
        };
        self.conn.execute(
            "DELETE FROM registry_crate WHERE registry_id = ?1 AND name = ?2",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn delete_registry_src(&mut self, registry: &str, name: &str) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "registry_index", registry)? else {
            return Ok(());
        };
        self.conn.execute(
            "DELETE FROM registry_src WHERE registry_id = ?1 AND name = ?2",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn delete_git_db(&mut self, name: &str) -> CargoResult<()> {
        self.conn
            .execute("DELETE FROM git_db WHERE name = ?1", [name])?;
        Ok(())
    }

    pub fn delete_git_checkout(&mut self, git_db: &str, name: &str) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "git_db", git_db)? else {
            return Ok(());
        };
        self.conn.execute(
            "DELETE FROM git_checkout WHERE git_id = ?1 AND name = ?2",
            params![id, name],
        )?;
        Ok(())
    }

    // --- Existence checks ---

    pub fn registry_src_exists(&self, registry: &str, name: &str) -> CargoResult<bool> {
        let Some(id) = Self::id_from_name(&self.conn, "registry_index", registry)? else {
            return Ok(false);
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT 1 FROM registry_src WHERE registry_id = ?1 AND name = ?2",
        )?;
        Ok(stmt.exists(params![id, name])?)
    }

    pub fn git_checkout_exists(&self, git_db: &str, name: &str) -> CargoResult<bool> {
        let Some(id) = Self::id_from_name(&self.conn, "git_db", git_db)? else {
            return Ok(false);
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT 1 FROM git_checkout WHERE git_id = ?1 AND name = ?2",
        )?;
        Ok(stmt.exists(params![id, name])?)
    }

    // --- Size updates ---

    pub fn update_registry_src_size(
        &mut self,
        registry: &str,
        name: &str,
        size: u64,
    ) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "registry_index", registry)? else {
            return Ok(());
        };
        self.conn.execute(
            "UPDATE registry_src SET size = ?1 WHERE registry_id = ?2 AND name = ?3",
            params![size, id, name],
        )?;
        Ok(())
    }

    pub fn update_git_checkout_size(
        &mut self,
        git_db: &str,
        name: &str,
        size: u64,
    ) -> CargoResult<()> {
        let Some(id) = Self::id_from_name(&self.conn, "git_db", git_db)? else {
            return Ok(());
        };
        self.conn.execute(
            "UPDATE git_checkout SET size = ?1 WHERE git_id = ?2 AND name = ?3",
            params![size, id, name],
        )?;
        Ok(())
    }

    // --- Transaction control ---

    pub fn begin(&mut self) -> CargoResult<()> {
        self.conn.execute_batch("BEGIN EXCLUSIVE")?;
        Ok(())
    }

    pub fn commit(&mut self) -> CargoResult<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&mut self) -> CargoResult<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    // --- Batch save from DeferredGlobalLastUse ---

    pub fn save_deferred(&mut self, deferred: &mut DeferredGlobalLastUse) -> CargoResult<()> {
        self.conn.execute_batch("BEGIN")?;

        // Parent tables first.
        let registry_index_timestamps = std::mem::take(&mut deferred.registry_index_timestamps);
        upsert_parent_entries(
            &self.conn,
            "registry_index",
            registry_index_timestamps
                .into_iter()
                .map(|(idx, ts)| (idx.encoded_registry_name, ts)),
            &mut self.registry_keys,
        )?;

        let git_db_timestamps = std::mem::take(&mut deferred.git_db_timestamps);
        upsert_parent_entries(
            &self.conn,
            "git_db",
            git_db_timestamps
                .into_iter()
                .map(|(db, ts)| (db.encoded_git_name, ts)),
            &mut self.git_keys,
        )?;

        // Child tables.
        let registry_crate_timestamps = std::mem::take(&mut deferred.registry_crate_timestamps);
        for (registry_crate, timestamp) in registry_crate_timestamps {
            trace!(target: "gc", "insert registry crate {registry_crate:?} {timestamp}");
            let registry_id = self.registry_id(registry_crate.encoded_registry_name)?;
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO registry_crate (registry_id, name, size, timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO UPDATE SET timestamp=excluded.timestamp
                    WHERE timestamp < ?5",
            )?;
            stmt.execute(params![
                registry_id,
                registry_crate.crate_filename,
                registry_crate.size,
                timestamp,
                timestamp - UPDATE_RESOLUTION
            ])?;
        }

        let registry_src_timestamps = std::mem::take(&mut deferred.registry_src_timestamps);
        for (registry_src, timestamp) in registry_src_timestamps {
            trace!(target: "gc", "insert registry src {registry_src:?} {timestamp}");
            let registry_id = self.registry_id(registry_src.encoded_registry_name)?;
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO registry_src (registry_id, name, size, timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO UPDATE SET timestamp=excluded.timestamp
                    WHERE timestamp < ?5",
            )?;
            stmt.execute(params![
                registry_id,
                registry_src.package_dir,
                registry_src.size,
                timestamp,
                timestamp - UPDATE_RESOLUTION
            ])?;
        }

        let git_checkout_timestamps = std::mem::take(&mut deferred.git_checkout_timestamps);
        for (git_checkout, timestamp) in git_checkout_timestamps {
            let git_id = self.git_id(git_checkout.encoded_git_name)?;
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO git_checkout (git_id, name, size, timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT DO UPDATE SET timestamp=excluded.timestamp
                    WHERE timestamp < ?5",
            )?;
            stmt.execute(params![
                git_id,
                git_checkout.short_name,
                git_checkout.size,
                timestamp,
                timestamp - UPDATE_RESOLUTION
            ])?;
        }

        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    // --- Error classification ---

    pub fn is_silent_error(e: &anyhow::Error) -> bool {
        if let Some(e) = e.downcast_ref::<rusqlite::Error>() {
            if matches!(
                e.sqlite_error_code(),
                Some(ErrorCode::CannotOpen | ErrorCode::ReadOnly)
            ) {
                return true;
            }
        }
        false
    }
}
