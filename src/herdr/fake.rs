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

pub type StartObserver = Box<dyn Fn(&str) + Send>;

pub struct FakeHerdr {
    pub on_start: RefCell<Option<StartObserver>>,
    pub on_prompt: RefCell<Option<StartObserver>>,
    pub on_send_keys: RefCell<Option<StartObserver>>,
    pub on_get: RefCell<Option<StartObserver>>,
    pub workspaces: RefCell<Vec<Workspace>>,
    pub calls: RefCell<Vec<String>>,
    pub notify_error: Cell<bool>,
    pub agent_records: RefCell<Vec<AgentInfo>>,
    pub agent_get_results: RefCell<VecDeque<Option<AgentInfo>>>,
    pub start_results: RefCell<VecDeque<AgentStart>>,
    /// Script transport outcomes independently of application acceptance.
    pub prompt_results: RefCell<VecDeque<(bool, Result<(), ShepherdError>)>>,
    pub accepted_prompts: RefCell<Vec<String>>,
    pub visible_screens: RefCell<VecDeque<String>>,
    pub visible_cache: RefCell<HashMap<String, String>>,
    pub read_errors: RefCell<HashMap<String, String>>,
    pub panic_on_start: Cell<bool>,
    pub start_args: RefCell<Vec<Vec<String>>>,
    pub start_envs: RefCell<Vec<HashMap<String, String>>>,
    pub pane_cwds: RefCell<HashMap<String, std::path::PathBuf>>,
    pub pane_sessions: RefCell<HashMap<String, String>>,
    pub pane_workspaces: RefCell<HashMap<String, String>>,
    /// Opt in to actual isolated Git resources for end-to-end fixtures.
    pub git_worktrees: Cell<bool>,
    pub move_error: Cell<bool>,
}

impl FakeHerdr {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            on_start: RefCell::new(None),
            on_prompt: RefCell::new(None),
            on_send_keys: RefCell::new(None),
            on_get: RefCell::new(None),
            workspaces: RefCell::new(workspaces),
            calls: RefCell::new(Vec::new()),
            notify_error: Cell::new(false),
            agent_records: RefCell::new(Vec::new()),
            agent_get_results: RefCell::new(VecDeque::new()),
            start_results: RefCell::new(VecDeque::new()),
            prompt_results: RefCell::new(VecDeque::new()),
            accepted_prompts: RefCell::new(Vec::new()),
            visible_screens: RefCell::new(VecDeque::new()),
            visible_cache: RefCell::new(HashMap::new()),
            read_errors: RefCell::new(HashMap::new()),
            panic_on_start: Cell::new(false),
            start_args: RefCell::new(Vec::new()),
            start_envs: RefCell::new(Vec::new()),
            pane_cwds: RefCell::new(HashMap::new()),
            pane_sessions: RefCell::new(HashMap::new()),
            pane_workspaces: RefCell::new(HashMap::new()),
            git_worktrees: Cell::new(false),
            move_error: Cell::new(false),
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
        self.pane_cwds
            .borrow_mut()
            .insert(format!("{ws}:p{sequence}"), cwd.into());
        self.pane_workspaces
            .borrow_mut()
            .insert(format!("{ws}:p{sequence}"), ws.into());
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
        if self.git_worktrees.get() {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["worktree", "add", "-b", branch])
                .arg(path)
                .arg(base)
                .output()
                .map_err(anyhow::Error::from)?;
            if !output.status.success() {
                return Err(anyhow::anyhow!(
                    "fake git worktree add: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
        }
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
        self.start_args.borrow_mut().push(args.to_vec());
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
            cwd: self.pane_cwds.borrow().get(pane).cloned(),
            pane_id: pane.into(),
            workspace_id: self.pane_workspaces.borrow().get(pane).cloned(),
        });
        if let Some(observer) = self.on_start.borrow().as_ref() {
            observer(name);
        }
        Ok(result)
    }
    fn agent_get(&self, name: &str) -> Result<Option<AgentInfo>, ShepherdError> {
        let info = self
            .agent_get_results
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| {
                self.agent_records
                    .borrow()
                    .iter()
                    .find(|agent| agent.name.as_deref() == Some(name))
                    .cloned()
            });
        if let Some(observer) = self.on_get.borrow().as_ref() {
            observer(name);
        }
        Ok(info)
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
        if let Some(observer) = self.on_prompt.borrow().as_ref() {
            observer(name);
        }
        if self
            .agent_records
            .borrow()
            .iter()
            .any(|agent| agent.name.as_deref() == Some(name) && agent.agent_status == "blocked")
        {
            return Err(ShepherdError::HerdrCommand {
                code: "agent_blocked".into(),
                message: "agent is blocked".into(),
            });
        }
        let (accepted, result) = self
            .prompt_results
            .borrow_mut()
            .pop_front()
            .unwrap_or((true, Ok(())));
        if accepted {
            self.accepted_prompts.borrow_mut().push(name.into());
        }
        result
    }
    fn send_keys(&self, name: &str, keys: &[String]) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("send_keys {name} {keys:?}"));
        if let Some(observer) = self.on_send_keys.borrow().as_ref() {
            observer(name);
        }
        Ok(())
    }
    fn read_visible(&self, name: &str) -> Result<String, ShepherdError> {
        self.calls.borrow_mut().push(format!("read_visible {name}"));
        if let Some(error) = self.read_errors.borrow().get(name) {
            return Err(anyhow::anyhow!("{error}").into());
        }
        if let Some(screen) = self.visible_screens.borrow_mut().pop_front() {
            self.visible_cache
                .borrow_mut()
                .insert(name.into(), screen.clone());
            return Ok(screen);
        }
        Ok(self
            .visible_cache
            .borrow()
            .get(name)
            .cloned()
            .unwrap_or_default())
    }
    fn close_tab(&self, tab: &str) -> Result<(), ShepherdError> {
        self.calls.borrow_mut().push(format!("close_tab {tab}"));
        let panes: Vec<String> = self
            .pane_cwds
            .borrow()
            .keys()
            .filter(|pane| pane.replace(":p", ":t") == tab)
            .cloned()
            .collect();
        self.agent_records
            .borrow_mut()
            .retain(|agent| agent.pane_id.replace(":p", ":t") != tab);
        for pane in panes {
            self.pane_cwds.borrow_mut().remove(&pane);
            self.pane_workspaces.borrow_mut().remove(&pane);
            self.pane_sessions.borrow_mut().remove(&pane);
        }
        Ok(())
    }
    fn focus_agent(&self, name: &str) -> Result<(), ShepherdError> {
        self.calls.borrow_mut().push(format!("focus_agent {name}"));
        Ok(())
    }
    fn pane_cwd(&self, pane: &str) -> Result<Option<std::path::PathBuf>, ShepherdError> {
        Ok(self.pane_cwds.borrow().get(pane).cloned())
    }
    fn pane_session_ref(&self, pane: &str) -> Result<Option<String>, ShepherdError> {
        Ok(self.pane_sessions.borrow().get(pane).cloned())
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
        if self.move_error.get() {
            return Err(anyhow::anyhow!("simulated regroup failure").into());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_agent_retains_created_tab_workspace_and_cwd() {
        let h = FakeHerdr::new(vec![]);
        let (_, pane) = h
            .create_tab("ticket:workspace", "coder", Path::new("/child"))
            .unwrap();
        h.start_agent("coder", "codex", &pane, &[], &HashMap::new())
            .unwrap();
        let info = h.agent_get("coder").unwrap().unwrap();
        assert_eq!(info.workspace_id.as_deref(), Some("ticket:workspace"));
        assert_eq!(info.cwd.as_deref(), Some(Path::new("/child")));
    }
}
