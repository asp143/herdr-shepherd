use std::cell::{Cell, RefCell};

use crate::error::ShepherdError;

use super::{
    Herdr,
    model::{AgentInfo, Workspace},
};

pub struct FakeHerdr {
    pub workspaces: RefCell<Vec<Workspace>>,
    pub calls: RefCell<Vec<String>>,
    pub notify_error: Cell<bool>,
}

impl FakeHerdr {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            workspaces: RefCell::new(workspaces),
            calls: RefCell::new(Vec::new()),
            notify_error: Cell::new(false),
        }
    }
}

impl Herdr for FakeHerdr {
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
        Ok(Vec::new())
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
