//! Ticket resource ownership across external work and state transactions.
//!
//! Lock order: revision dispatch, ticket operation, spawn placement, Git
//! integration or agent/prompt operation, then short project-store transactions.
//! Never acquire dispatch while holding an operation guard. Shared guards may
//! nest (CLI placement -> launch -> prompt); each opens its own file description.
//! Cancellation and integration take exclusive guards before their snapshots,
//! and retain them through retirement/persistence/cleanup. No global state lock
//! is held while waiting on Herdr or Git. Nonblocking agent reservations may
//! run inside a store transaction; they never wait in the reverse order.

use std::{fs::File, path::Path};

use crate::{error::ShepherdError, store::Store};

pub struct Guard {
    file: File,
}
impl Drop for Guard {
    fn drop(&mut self) {
        // Release even if a concurrently forked child inherited the description.
        let _ = self.file.unlock();
    }
}

fn acquire(store: &Store, id: &str, shared: bool) -> Result<Guard, ShepherdError> {
    std::fs::create_dir_all(store.root()).map_err(anyhow::Error::from)?;
    let identity = crate::store::project_hash(Path::new(id));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.root().join(format!("ticket-{identity}.lock")))
        .map_err(anyhow::Error::from)?;
    if shared {
        file.lock_shared()
    } else {
        file.lock()
    }
    .map_err(anyhow::Error::from)?;
    Ok(Guard { file })
}

/// Admit work concurrently, excluding terminal transitions and integration.
pub fn shared(store: &Store, id: &str) -> Result<Guard, ShepherdError> {
    acquire(store, id, true)
}

/// Drain admitted work before observing or changing ticket resource ownership.
pub fn exclusive(store: &Store, id: &str) -> Result<Guard, ShepherdError> {
    acquire(store, id, false)
}
