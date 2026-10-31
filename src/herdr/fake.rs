use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    path::Path,
};

use crate::error::ShepherdError;

use super::{
    AgentStart, Herdr, WorktreeInfo,
    model::{AgentInfo, Workspace},
};

pub struct FakeHerdr {
    pub workspaces: RefCell<Vec<Workspace>>,
    pub calls: RefCell<Vec<String>>,
    pub notify_error: Cell<bool>,
    pub agent_records: RefCell<Vec<AgentInfo>>,
    pub start_results: RefCell<VecDeque<AgentStart>>,
    pub visible_screens: RefCell<VecDeque<String>>,
    pub panic_on_start: Cell<bool>,
    pub start_envs: RefCell<Vec<HashMap<String, String>>>,
}

impl FakeHerdr {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            workspaces: RefCell::new(workspaces),
            calls: RefCell::new(Vec::new()),
            notify_error: Cell::new(false),
            agent_records: RefCell::new(Vec::new()),
            start_results: RefCell::new(VecDeque::new()),
            visible_screens: RefCell::new(VecDeque::new()),
            panic_on_start: Cell::new(false),
            start_envs: RefCell::new(Vec::new()),
        }
    }
}

impl Herdr for FakeHerdr {
    fn create_tab(
        &self,
        ws: &str,
        label: &str,
        cwd: &Path,
    ) -> Result<(String, String), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("create_tab {ws} {label} {}", cwd.display()));
        let sequence = self.calls.borrow().len();
        Ok((format!("{ws}:t{sequence}"), format!("{ws}:p{sequence}")))
    }
    fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        label: &str,
        base: &str,
        path: &Path,
    ) -> Result<Workspace, ShepherdError> {
        self.calls.borrow_mut().push(format!(
            "create_worktree {} {branch} {label} {base} {}",
            repo.display(),
            path.display()
        ));
        let workspace = Workspace {
            workspace_id: format!("fake-w{}", self.workspaces.borrow().len() + 1),
            label: label.into(),
            worktree: Some(WorktreeInfo {
                repo_key: repo.display().to_string(),
                repo_root: Some(repo.into()),
                checkout_path: Some(path.into()),
                is_linked_worktree: true,
            }),
        };
        self.workspaces.borrow_mut().push(workspace.clone());
        Ok(workspace)
    }
    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<AgentStart, ShepherdError> {
        assert!(!self.panic_on_start.get(), "simulated crash during start");
        self.calls
            .borrow_mut()
            .push(format!("start_agent {name} {kind} {pane} -- {args:?}"));
        self.start_envs.borrow_mut().push(env.clone());
        let result = self
            .start_results
            .borrow_mut()
            .pop_front()
            .unwrap_or(AgentStart::Ready);
        self.agent_records.borrow_mut().push(AgentInfo {
            name: Some(name.into()),
            agent: kind.into(),
            agent_status: if result == AgentStart::Ready {
                "idle"
            } else {
                "blocked"
            }
            .into(),
            cwd: None,
            pane_id: pane.into(),
            workspace_id: None,
        });
        Ok(result)
    }
    fn prompt_agent(
        &self,
        name: &str,
        text: &str,
        until_working: bool,
    ) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("prompt_agent {name} {text} {until_working}"));
        if self
            .agent_records
            .borrow()
            .iter()
            .any(|agent| agent.name.as_deref() == Some(name) && agent.agent_status == "blocked")
        {
            return Err(anyhow::anyhow!("agent_blocked").into());
        }
        Ok(())
    }
    fn send_keys(&self, name: &str, keys: &[String]) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("send_keys {name} {keys:?}"));
        Ok(())
    }
    fn read_visible(&self, name: &str) -> Result<String, ShepherdError> {
        self.calls.borrow_mut().push(format!("read_visible {name}"));
        Ok(self
            .visible_screens
            .borrow_mut()
            .pop_front()
            .unwrap_or_default())
    }
    fn close_tab(&self, tab: &str) -> Result<(), ShepherdError> {
        self.calls.borrow_mut().push(format!("close_tab {tab}"));
        self.agent_records
            .borrow_mut()
            .retain(|agent| agent.pane_id.replace(":p", ":t") != tab);
        Ok(())
    }
    fn focus_agent(&self, name: &str) -> Result<(), ShepherdError> {
        self.calls.borrow_mut().push(format!("focus_agent {name}"));
        Ok(())
    }
    fn resume_agent(&self, name: &str) -> Result<(), ShepherdError> {
        self.calls.borrow_mut().push(format!("resume_agent {name}"));
        Ok(())
    }
    fn report_workspace_tokens(
        &self,
        ws: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("workspace_tokens {ws} {set:?} {clear:?}"));
        Ok(())
    }

    fn report_pane_tokens(
        &self,
        pane: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("pane_tokens {pane} {set:?} {clear:?}"));
        Ok(())
    }

    fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("notify {title} {body}"));
        if self.notify_error.get() {
            Err(ShepherdError::Runtime(anyhow::anyhow!(
                "notification unavailable"
            )))
        } else {
            Ok(())
        }
    }

    fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("remove_worktree {ws} {force}"));
        self.workspaces
            .borrow_mut()
            .retain(|workspace| workspace.workspace_id != ws);
        Ok(())
    }

    fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("close_workspace {ws}"));
        self.workspaces
            .borrow_mut()
            .retain(|workspace| workspace.workspace_id != ws);
        Ok(())
    }

    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        self.calls.borrow_mut().push("workspaces".into());
        Ok(self.workspaces.borrow().clone())
    }

    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        self.calls.borrow_mut().push("agents".into());
        Ok(self.agent_records.borrow().clone())
    }

    fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
        self.calls.borrow_mut().push(format!("rename {id} {label}"));
        if let Some(workspace) = self
            .workspaces
            .borrow_mut()
            .iter_mut()
            .find(|workspace| workspace.workspace_id == id)
        {
            workspace.label = label.into();
        }
        Ok(())
    }

    fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("move {ids:?} {before:?}"));
        let mut workspaces = self.workspaces.borrow_mut();
        let moved: Vec<Workspace> = ids
            .iter()
            .filter_map(|id| {
                workspaces
                    .iter()
                    .find(|workspace| &workspace.workspace_id == id)
                    .cloned()
            })
            .collect();
        workspaces.retain(|workspace| !ids.contains(&workspace.workspace_id));
        let position = before
            .and_then(|before| {
                workspaces
                    .iter()
                    .position(|workspace| workspace.workspace_id == before)
            })
            .unwrap_or(workspaces.len());
        workspaces.splice(position..position, moved);
        Ok(())
    }
}
