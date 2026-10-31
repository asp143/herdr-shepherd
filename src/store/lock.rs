use std::{
    fs::{File, OpenOptions},
    path::Path,
};

/// Each acquisition opens a distinct file description, so threads and processes
/// both contend on the same OS lock. Closing the file releases it on every path.
pub(super) struct Lock {
    _file: File,
}

impl Lock {
    pub(super) fn acquire(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}
