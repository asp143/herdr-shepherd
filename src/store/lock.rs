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
    pub(super) fn acquire_shared(path: &Path) -> std::io::Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?,
            Err(err) => return Err(err),
        };
        file.lock_shared()?;
        Ok(Self { _file: file })
    }

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
