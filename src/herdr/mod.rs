pub mod cli;
pub mod model;
pub mod socket;

pub use cli::CliHerdr;
pub use model::{AgentInfo, Workspace, WorktreeInfo};

use std::{collections::HashMap, path::Path};

use crate::error::ShepherdError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStart {
    Ready,
    NotReady,
}

fn unsupported(operation: &str) -> ShepherdError {
    anyhow::anyhow!("Herdr adapter does not support {operation}").into()
}

pub trait Herdr {
    fn create_tab(
        &self,
        _ws: &str,
        _label: &str,
        _cwd: &Path,
    ) -> Result<(String, String), ShepherdError> {
        Err(unsupported("create_tab"))
    }
    fn create_worktree(
        &self,
        _repo: &Path,
        _branch: &str,
        _label: &str,
        _base: &str,
        _path: &Path,
    ) -> Result<Workspace, ShepherdError> {
        Err(unsupported("create_worktree"))
    }
    fn start_agent(
        &self,
        _name: &str,
        _kind: &str,
        _pane: &str,
        _args: &[String],
        _env: &HashMap<String, String>,
    ) -> Result<AgentStart, ShepherdError> {
        Err(unsupported("start_agent"))
    }
    fn prompt_agent(
        &self,
        _name: &str,
        _text: &str,
        _until_working: bool,
    ) -> Result<(), ShepherdError> {
        Err(unsupported("prompt_agent"))
    }
    fn send_keys(&self, _name: &str, _keys: &[String]) -> Result<(), ShepherdError> {
        Err(unsupported("send_keys"))
    }
    fn read_visible(&self, _name: &str) -> Result<String, ShepherdError> {
        Err(unsupported("read_visible"))
    }
    fn agent_get(&self, name: &str) -> Result<Option<AgentInfo>, ShepherdError> {
        Ok(self
            .agents()?
            .into_iter()
            .find(|agent| agent.name.as_deref() == Some(name)))
    }
    fn close_tab(&self, _tab: &str) -> Result<(), ShepherdError> {
        Err(unsupported("close_tab"))
    }
    fn focus_agent(&self, _name: &str) -> Result<(), ShepherdError> {
        Err(unsupported("focus_agent"))
    }
    fn resume_agent(&self, _name: &str) -> Result<(), ShepherdError> {
        Err(unsupported("resume_agent"))
    }
    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError>;
    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError>;
    fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError>;
    fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError>;
    fn report_workspace_tokens(
        &self,
        ws: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError>;
    fn report_pane_tokens(
        &self,
        pane: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError>;
    fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError>;
    fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError>;
    fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError>;
}
#[cfg(any(test, debug_assertions, feature = "test-support"))]
pub mod fake;
