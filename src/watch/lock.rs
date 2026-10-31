use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use crate::error::ShepherdError;

pub struct LockGuard {
    _file: File,
}

pub fn acquire(state_dir: &Path) -> Result<Option<LockGuard>, ShepherdError> {
    acquire_named(state_dir, "watch.lock")
}

pub(super) fn acquire_named(
    state_dir: &Path,
    name: &str,
) -> Result<Option<LockGuard>, ShepherdError> {
    fs::create_dir_all(state_dir).map_err(anyhow::Error::from)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(state_dir.join(name))
        .map_err(anyhow::Error::from)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(LockGuard { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(error) => Err(anyhow::Error::from(error).into()),
    }
}
