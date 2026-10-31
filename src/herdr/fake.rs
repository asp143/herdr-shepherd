use std::cell::RefCell;

use crate::error::ShepherdError;

use super::{
    Herdr,
    model::{AgentInfo, Workspace},
};

pub struct FakeHerdr {
    pub workspaces: RefCell<Vec<Workspace>>,
    pub calls: RefCell<Vec<String>>,
}

impl FakeHerdr {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            workspaces: RefCell::new(workspaces),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl Herdr for FakeHerdr {
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
