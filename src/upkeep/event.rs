use serde::Serialize;

use crate::{config::Config, error::ShepherdError, herdr::Herdr};

use super::regroup::{self, RegroupReport};

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOutcome {
    Regrouped(RegroupReport),
    Disabled,
    Ignored,
}

pub fn handle_event(
    name: &str,
    _payload: &serde_json::Value,
    cfg: &Config,
    h: &dyn Herdr,
) -> Result<EventOutcome, ShepherdError> {
    if !matches!(name, "worktree.created" | "worktree.removed") {
        return Ok(EventOutcome::Ignored);
    }
    if !cfg.upkeep.regroup {
        return Ok(EventOutcome::Disabled);
    }
    let workspaces = h.workspaces()?;
    let plan = regroup::plan_regroup(&workspaces);
    regroup::apply(h, &plan).map(EventOutcome::Regrouped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        herdr::{
            fake::FakeHerdr,
            model::{Workspace, WorktreeInfo},
        },
    };

    fn unpacked() -> FakeHerdr {
        let workspace = |id: &str, label: &str, repo: &str, child: bool| Workspace {
            workspace_id: id.into(),
            label: label.into(),
            worktree: Some(WorktreeInfo {
                repo_key: repo.into(),
                repo_root: None,
                checkout_path: None,
                is_linked_worktree: child,
            }),
        };
        FakeHerdr::new(vec![
            workspace("P", "project", "repo", false),
            workspace("X", "other", "other", false),
            workspace("C", "worktree/task", "repo", true),
        ])
    }

    #[test]
    fn regroups_on_worktree_created() {
        let config_dir = tempfile::tempdir().unwrap();
        let cfg = config::load(config_dir.path(), None).unwrap();
        let h = unpacked();
        let outcome = handle_event("worktree.created", &serde_json::json!({}), &cfg, &h).unwrap();
        assert!(matches!(
            outcome,
            EventOutcome::Regrouped(RegroupReport {
                relabeled: 1,
                repacked: 1
            })
        ));
    }

    #[test]
    fn disabled_when_upkeep_off() {
        let config_dir = tempfile::tempdir().unwrap();
        let mut cfg = config::load(config_dir.path(), None).unwrap();
        cfg.upkeep.regroup = false;
        let h = unpacked();
        assert!(matches!(
            handle_event("worktree.created", &serde_json::json!({}), &cfg, &h).unwrap(),
            EventOutcome::Disabled
        ));
        assert!(h.calls.borrow().is_empty());
    }

    #[test]
    fn ignores_other_events() {
        let config_dir = tempfile::tempdir().unwrap();
        let cfg = config::load(config_dir.path(), None).unwrap();
        let h = unpacked();
        assert!(matches!(
            handle_event("pane.created", &serde_json::json!({}), &cfg, &h).unwrap(),
            EventOutcome::Ignored
        ));
        assert!(h.calls.borrow().is_empty());
    }

    #[test]
    fn regroups_on_worktree_removed() {
        let config_dir = tempfile::tempdir().unwrap();
        let cfg = config::load(config_dir.path(), None).unwrap();
        let h = unpacked();
        assert!(matches!(
            handle_event("worktree.removed", &serde_json::json!({}), &cfg, &h).unwrap(),
            EventOutcome::Regrouped(RegroupReport { repacked: 1, .. })
        ));
    }
}
