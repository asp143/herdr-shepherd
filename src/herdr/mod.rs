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

/// Identity observed for an existing harness. Callers must hold admission locks
/// while reserving the assignment and submitting it to this exact recipient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRecipient {
    pub name: String,
    pub harness: String,
    pub pane_id: String,
    pub session_ref: String,
}

/// Only Accepted acknowledges the waited prompt. Ambiguous must never trigger
/// an automatic retry or replacement writer until the old recipient is stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskDelivery {
    Accepted,
    Rejected { reason: String },
    Dead { reason: String },
    Ambiguous { reason: String },
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
    /// Submit a new assignment, rather than an ordinary followup or nudge.
    /// Err means preflight failed before any input was sent. Once submission
    /// begins, every outcome (including a transport error) is a TaskDelivery.
    /// This does not reserve assignments: callers must serialize dispatch and
    /// persist task intent first. It never retries, starts or stops a process.
    fn deliver_task(
        &self,
        recipient: &TaskRecipient,
        text: &str,
    ) -> Result<TaskDelivery, ShepherdError> {
        if text.trim().is_empty() {
            return Ok(TaskDelivery::Rejected {
                reason: "empty task".into(),
            });
        }
        if crate::harness::harness_for(&recipient.harness).is_err() {
            return Ok(TaskDelivery::Rejected {
                reason: "unsupported harness".into(),
            });
        }
        let Some(info) = self.agent_get(&recipient.name)? else {
            return Ok(TaskDelivery::Dead {
                reason: "recipient no longer exists".into(),
            });
        };
        if info.name.as_deref() != Some(recipient.name.as_str())
            || info.agent != recipient.harness
            || info.pane_id != recipient.pane_id
            || recipient.pane_id.is_empty()
            || recipient.session_ref.is_empty()
            || self.pane_session_ref(&recipient.pane_id)?.as_deref()
                != Some(recipient.session_ref.as_str())
        {
            return Ok(TaskDelivery::Rejected {
                reason: "recipient identity or session changed".into(),
            });
        }
        if !matches!(info.agent_status.as_str(), "idle" | "done") {
            return Ok(TaskDelivery::Rejected {
                reason: "recipient is not idle or done".into(),
            });
        }
        Ok(match self.prompt_agent(&recipient.name, text, true) {
            Ok(()) => TaskDelivery::Accepted,
            Err(error) => {
                // Unknown failures can follow a partial PTY write or acceptance.
                // Preserve only the protocol code, never prompt/server prose.
                let code = error.herdr_code().unwrap_or("transport_error");
                let reason = format!("task prompt: {code}");
                match code {
                    "agent_blocked" | "agent_not_ready" | "empty_agent_prompt" => {
                        TaskDelivery::Rejected { reason }
                    }
                    "agent_not_found" | "not_found" | "agent_target_not_found" => {
                        TaskDelivery::Dead { reason }
                    }
                    _ => TaskDelivery::Ambiguous { reason },
                }
            }
        })
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
    fn pane_cwd(&self, _pane: &str) -> Result<Option<std::path::PathBuf>, ShepherdError> {
        Ok(None)
    }
    /// Distinguish a closed pane from a present pane with unavailable metadata.
    fn pane_exists(&self, _pane: &str) -> Result<bool, ShepherdError> {
        Err(unsupported("pane_exists"))
    }
    fn pane_session_ref(&self, _pane: &str) -> Result<Option<String>, ShepherdError> {
        Ok(None)
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

#[cfg(test)]
mod delivery_tests;
