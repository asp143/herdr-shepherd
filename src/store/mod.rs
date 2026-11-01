mod lock;
pub mod model;

pub use model::*;

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::error::ShepherdError;

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    project_root: PathBuf,
}

/// Hash the project's filesystem path without lossy UTF-8 conversion.
pub fn project_hash(project_root: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(project_root.as_os_str().as_encoded_bytes())
    )
}

impl Store {
    pub fn open(state_dir: &Path, project_root: &Path) -> Result<Self, ShepherdError> {
        let project_root = fs::canonicalize(project_root).context("resolve project root")?;
        let root = state_dir.join("projects").join(project_hash(&project_root));
        Ok(Self { root, project_root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn read(&self) -> Result<StoreData, ShepherdError> {
        let _lock = match lock::Lock::acquire_shared(&self.root.join("state.lock")) {
            Ok(lock) => lock,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(self.empty_data()),
            Err(err) => return Err(anyhow::Error::new(err).context("lock project state").into()),
        };
        self.read_unlocked()
    }

    /// Read and validate state without creating a directory or lock file.
    /// Atomic state replacement also permits reading when no lock exists yet.
    pub fn read_only(&self) -> Result<StoreData, ShepherdError> {
        let _lock = match lock::Lock::acquire_shared_existing(&self.root.join("state.lock")) {
            Ok(lock) => Some(lock),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(anyhow::Error::new(err).context("lock project state").into()),
        };
        self.read_unlocked()
    }

    pub fn update<T>(
        &self,
        f: impl FnOnce(&mut StoreData) -> Result<T, ShepherdError>,
    ) -> Result<T, ShepherdError> {
        fs::create_dir_all(&self.root).context("create project state directory")?;
        let _lock =
            lock::Lock::acquire(&self.root.join("state.lock")).context("lock project state")?;
        let mut data = self.read_unlocked()?;
        let result = f(&mut data)?;
        self.validate(&data)?;
        self.write_unlocked(&data)?;
        Ok(result)
    }

    /// Heal with the same state lock as enqueue; routine cleanup only logs counts.
    pub fn heal_pending_pings(&self) -> Result<usize, ShepherdError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let healed = self.update(|data| Ok(data.heal_pending_pings(now)))?;
        if healed > 0 {
            eprintln!("shepherd: healed {healed} queued notice(s)");
        }
        Ok(healed)
    }

    fn read_unlocked(&self) -> Result<StoreData, ShepherdError> {
        let path = self.root.join("state.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let recovery = || {
                    format!(
                        "invalid project state at {}; preserve the file and restore a known-good backup before retrying",
                        path.display()
                    )
                };
                let mut data: StoreData = serde_json::from_slice(&bytes).with_context(recovery)?;
                if data.schema_version == 1 {
                    data.schema_version = 2;
                }
                self.validate(&data).with_context(recovery)?;
                Ok(data)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(self.empty_data()),
            Err(err) => Err(anyhow::Error::new(err).context("read project state").into()),
        }
    }

    fn empty_data(&self) -> StoreData {
        StoreData {
            schema_version: 2,
            project_root: self.project_root.clone(),
            trust: None,
            lead: None,
            tickets: Vec::new(),
            agents: Vec::new(),
            pending_pings: Vec::new(),
            protection_cache: Default::default(),
        }
    }

    fn validate(&self, data: &StoreData) -> Result<(), ShepherdError> {
        if data.schema_version != 2 {
            return Err(anyhow::anyhow!(
                "unsupported store schema version {}",
                data.schema_version
            )
            .into());
        }
        if data.project_root != self.project_root {
            return Err(
                anyhow::anyhow!("stored project root does not match requested project").into(),
            );
        }
        Ok(())
    }

    fn write_unlocked(&self, data: &StoreData) -> Result<(), ShepherdError> {
        let bytes = serde_json::to_vec_pretty(data).context("encode project state")?;
        let temporary = self.root.join("state.json.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .context("open temporary project state")?;
        file.write_all(&bytes)
            .context("write temporary project state")?;
        file.sync_all().context("sync temporary project state")?;
        fs::rename(&temporary, self.root.join("state.json")).context("replace project state")?;
        File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .context("sync project state directory")?;
        Ok(())
    }
}
