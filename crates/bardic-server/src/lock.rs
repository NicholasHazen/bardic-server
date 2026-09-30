use fs4::fs_std::FileExt;
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

/// Holds an exclusive lock on the data folder for as long as it lives, so only
/// one server ever writes it. The operating system releases it if the process dies.
pub struct InstanceLock {
    _file: File,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another Bardic server is already using {0}")]
    InUse(String),
    #[error("cannot create the lock file: {0}")]
    Io(#[from] io::Error),
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        std::fs::create_dir_all(data_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data_dir.join("bardic.lock"))?;
        match file.try_lock_exclusive() {
            Ok(true) => Ok(InstanceLock { _file: file }),
            Ok(false) => Err(LockError::InUse(data_dir.display().to_string())),
            Err(e) => Err(LockError::Io(e)),
        }
    }
}
