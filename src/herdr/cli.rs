use std::{env, io, path::PathBuf, process::Command, time::Duration};

use serde_json::json;

use super::{AgentInfo, Herdr, Workspace, model, socket};
use crate::{error::ShepherdError, proc};

pub struct CliHerdr {
    pub bin: PathBuf,
    pub socket: PathBuf,
}

impl CliHerdr {
    pub fn from_env() -> Self {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
        Self {
            bin: env::var_os("HERDR_BIN_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("herdr")),
            socket: env::var_os("HERDR_SOCKET_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config/herdr/herdr.sock")),
        }
    }

    fn command(&self, args: &[&str]) -> Result<String, ShepherdError> {
        // Check connectivity rather than existence: stale sockets also mean the
        // server is unavailable, regardless of how the CLI reports its error.
        drop(socket::connect(&self.socket)?);
        match proc::run(
            Command::new(&self.bin)
                .env("HERDR_SOCKET_PATH", &self.socket)
                .args(args),
            Duration::from_secs(20),
        ) {
            Ok(Some(out)) => Ok(out),
            Ok(None) => {
                drop(socket::connect(&self.socket)?);
                Err(ShepherdError::Runtime(anyhow::anyhow!(
                    "herdr command failed or timed out: {}",
                    args.join(" ")
                )))
            }
            Err(err)
                if err
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                Err(ShepherdError::HerdrUnavailable(format!(
                    "herdr binary {} unavailable (socket {})",
                    self.bin.display(),
                    self.socket.display()
                )))
            }
            Err(err) => Err(ShepherdError::Runtime(err)),
        }
    }
}

impl Herdr for CliHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        model::parse_snapshot(&self.command(&["api", "snapshot"])?).map_err(ShepherdError::Runtime)
    }

    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        model::parse_agent_list(&self.command(&["agent", "list"])?).map_err(ShepherdError::Runtime)
    }

    fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
        self.command(&["workspace", "rename", id, label])
            .map(|_| ())
    }

    fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError> {
        socket::request(
            &self.socket,
            &json!({
                "id": "shepherd-move",
                "method": "workspace.move_block",
                "params": {"workspace_ids": ids, "before_workspace_id": before}
            }),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn missing_binary_is_herdr_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let h = CliHerdr {
            bin: dir.path().join("missing-herdr"),
            socket: socket.clone(),
        };
        let err = h.workspaces().unwrap_err();
        assert!(matches!(err, ShepherdError::HerdrUnavailable(_)));
        assert!(err.to_string().contains(socket.to_str().unwrap()));
    }

    #[test]
    fn stale_socket_is_herdr_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        drop(UnixListener::bind(&socket).unwrap());
        let h = CliHerdr {
            bin: PathBuf::from("herdr"),
            socket: socket.clone(),
        };
        let err = h.workspaces().unwrap_err();
        assert!(matches!(err, ShepherdError::HerdrUnavailable(_)));
        assert!(err.to_string().contains(socket.to_str().unwrap()));
    }
}
