use crate::error::ApiError;
use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Ordered migrations. `PRAGMA user_version` holds how many have run. Never edit
/// one that has shipped; add a new file.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_listeners.sql"),
    include_str!("../migrations/0003_library.sql"),
    include_str!("../migrations/0004_places.sql"),
    include_str!("../migrations/0005_voices.sql"),
    include_str!("../migrations/0006_audio.sql"),
];

/// Schema version a fresh or fully migrated database ends at.
pub fn latest_schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("this data folder was written by a newer Bardic (schema {found}, this server knows {known})")]
    TooNew { found: i64, known: i64 },
}

/// SQLite behind one connection. Every call is short and runs on the blocking
/// pool; nothing holds the connection across a provider request.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    data_dir: PathBuf,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Store, StoreError> {
        std::fs::create_dir_all(data_dir)?;
        let mut conn = Connection::open(data_dir.join("bardic.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn, data_dir)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            data_dir: data_dir.to_path_buf(),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Run a short database job on the blocking pool.
    pub async fn run<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        F: FnOnce(&mut Connection) -> Result<T, ApiError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| ApiError::internal("database lock poisoned"))?;
            f(&mut guard)
        })
        .await
        .map_err(ApiError::internal)?
    }

    /// Synchronous variant for start-up work, before the runtime serves requests.
    pub fn run_blocking<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut guard = self.conn.lock().expect("db lock");
        f(&mut guard)
    }

    pub fn schema_version(&self) -> i64 {
        let g = self.conn.lock().expect("db lock");
        g.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap_or(0)
    }
}

fn migrate(conn: &mut Connection, data_dir: &Path) -> Result<(), StoreError> {
    let known = MIGRATIONS.len() as i64;
    let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if found > known {
        return Err(StoreError::TooNew { found, known });
    }
    if found == known {
        return Ok(());
    }
    // A recoverable point before changing an existing database.
    if found > 0 {
        let dir = data_dir.join("backups");
        std::fs::create_dir_all(&dir)?;
        let target = dir.join(format!("pre-migration-{found}.db"));
        if target.exists() {
            std::fs::remove_file(&target)?;
        }
        conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}
