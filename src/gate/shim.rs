use crate::{error::ShepherdError, gate::Gate};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

pub fn install_shim(state_dir: &Path, shepherd_bin: &Path) -> Result<PathBuf, ShepherdError> {
    let shim = state_dir.join("shim");
    fs::create_dir_all(&shim).map_err(anyhow::Error::from)?;
    fs::create_dir_all(state_dir.join("agent-gh")).map_err(anyhow::Error::from)?;
    let shim = shim.canonicalize().map_err(anyhow::Error::from)?;
    let binary = shepherd_bin.canonicalize().map_err(anyhow::Error::from)?;
    let file = shim.join("gh");
    fs::write(&file, format!("#!/bin/sh\nSHEPHERD_SHIM_DIR={}\nexport SHEPHERD_SHIM_DIR\nexec {} shim gh -- \"$@\"\n", shell_quote(&shim), shell_quote(&binary))).map_err(anyhow::Error::from)?;
    fs::set_permissions(file, fs::Permissions::from_mode(0o700)).map_err(anyhow::Error::from)?;
    Ok(shim)
}

pub fn run(args: &[String], state_dir: &Path) -> Result<(), ShepherdError> {
    let shim = std::env::var_os("SHEPHERD_SHIM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("shim"));
    let shim = shim.canonicalize().unwrap_or(shim);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<_> = std::env::split_paths(&path)
        .filter(|entry| entry.canonicalize().unwrap_or_else(|_| entry.clone()) != shim)
        .collect();
    let path = std::env::join_paths(kept).map_err(anyhow::Error::from)?;
    Gate::exec_gh(&args.iter().map(String::as_str).collect::<Vec<_>>(), &path)
}
