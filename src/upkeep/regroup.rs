use serde::Serialize;

use crate::{
    error::ShepherdError,
    herdr::{Herdr, model::Workspace},
};

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct RegroupPlan {
    pub renames: Vec<(String, String)>,
    pub moves: Vec<Move>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Move {
    pub workspace_ids: Vec<String>,
    pub before: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RegroupReport {
    pub relabeled: usize,
    pub repacked: usize,
}

struct Group<'a> {
    repo_key: &'a str,
    parent: Option<&'a Workspace>,
    children: Vec<&'a Workspace>,
}

pub fn plan_regroup(workspaces: &[Workspace]) -> RegroupPlan {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for workspace in workspaces {
        let Some(worktree) = workspace.worktree.as_ref() else {
            continue;
        };
        let group_index = match groups
            .iter()
            .position(|group| group.repo_key == worktree.repo_key)
        {
            Some(index) => index,
            None => {
                groups.push(Group {
                    repo_key: &worktree.repo_key,
                    parent: None,
                    children: Vec::new(),
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[group_index];
        if worktree.is_linked_worktree {
            group.children.push(workspace);
        } else if group.parent.is_none() {
            group.parent = Some(workspace);
        }
    }
    let mut plan = RegroupPlan::default();
    let mut order: Vec<String> = workspaces
        .iter()
        .map(|workspace| workspace.workspace_id.clone())
        .collect();
    for group in groups {
        let Some(parent) = group.parent else {
            continue;
        };
        if group.children.is_empty() {
            continue;
        }
        let parent_prefix = format!("{}/", parent.label);
        for child in &group.children {
            let mut label = child.label.as_str();
            while let Some(bare) = label
                .strip_prefix("worktree/")
                .or_else(|| label.strip_prefix(&parent_prefix))
                .filter(|label| !label.is_empty())
            {
                label = bare;
            }
            if label != child.label {
                plan.renames
                    .push((child.workspace_id.clone(), label.to_owned()));
            }
        }
        let children: Vec<String> = group
            .children
            .iter()
            .map(|child| child.workspace_id.clone())
            .collect();
        let Some(parent_index) = order.iter().position(|id| id == &parent.workspace_id) else {
            continue;
        };
        if order.get(parent_index + 1..parent_index + 1 + children.len())
            == Some(children.as_slice())
        {
            continue;
        }
        let before = order
            .iter()
            .skip(parent_index + 1)
            .find(|id| !children.contains(id))
            .cloned();
        order.retain(|id| !children.contains(id));
        let insert_index = before
            .as_ref()
            .and_then(|before| order.iter().position(|id| id == before))
            .unwrap_or(order.len());
        order.splice(insert_index..insert_index, children.iter().cloned());
        plan.moves.push(Move {
            workspace_ids: children,
            before,
        });
    }
    plan
}

pub fn apply(h: &dyn Herdr, plan: &RegroupPlan) -> Result<RegroupReport, ShepherdError> {
    for (id, label) in &plan.renames {
        h.rename_workspace(id, label)?;
    }
    for movement in &plan.moves {
        h.move_block(&movement.workspace_ids, movement.before.as_deref())?;
    }
    Ok(RegroupReport {
        relabeled: plan.renames.len(),
        repacked: plan.moves.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{
        Herdr,
        fake::FakeHerdr,
        model::{Workspace, WorktreeInfo},
    };

    fn ws(id: &str, label: &str, repo: &str, child: bool) -> Workspace {
        Workspace {
            workspace_id: id.into(),
            label: label.into(),
            worktree: Some(WorktreeInfo {
                repo_key: repo.into(),
                repo_root: None,
                checkout_path: None,
                is_linked_worktree: child,
            }),
        }
    }

    fn parent(id: &str) -> Workspace {
        ws(id, "rff", id, false)
    }
    fn child(id: &str, label: &str, repo: &str) -> Workspace {
        ws(id, label, repo, true)
    }

    #[test]
    fn strips_worktree_prefix() {
        let plan = plan_regroup(&[parent("P"), child("C", "worktree/fix-foo", "P")]);
        assert_eq!(plan.renames, [("C".into(), "fix-foo".into())]);
    }

    #[test]
    fn strips_parent_prefix() {
        let plan = plan_regroup(&[parent("P"), child("C", "rff/ppcv", "P")]);
        assert_eq!(plan.renames, [("C".into(), "ppcv".into())]);
    }

    #[test]
    fn leaves_user_slash_labels() {
        let plan = plan_regroup(&[parent("P"), child("C", "fix/foo", "P")]);
        assert!(plan.renames.is_empty());
    }

    #[test]
    fn packs_children_after_parent() {
        let plan = plan_regroup(&[
            parent("P"),
            parent("X"),
            child("C1", "one", "P"),
            child("C2", "two", "P"),
        ]);
        assert_eq!(
            plan.moves,
            [Move {
                workspace_ids: vec!["C1".into(), "C2".into()],
                before: Some("X".into())
            }]
        );
    }

    #[test]
    fn already_packed_is_noop() {
        let plan = plan_regroup(&[
            parent("P"),
            child("C1", "one", "P"),
            child("C2", "two", "P"),
            parent("X"),
        ]);
        assert!(plan.moves.is_empty());
        assert!(plan.renames.is_empty());
    }

    #[test]
    fn orphan_group_skipped() {
        let plan = plan_regroup(&[child("C", "worktree/orphan", "absent"), parent("X")]);
        assert!(plan.moves.is_empty());
        assert!(plan.renames.is_empty());
    }

    #[test]
    fn second_run_is_noop() {
        let h = FakeHerdr::new(vec![
            parent("P"),
            parent("X"),
            child("C", "worktree/fix-foo", "P"),
        ]);
        let first = plan_regroup(&h.workspaces().unwrap());
        let report = apply(&h, &first).unwrap();
        assert_eq!(
            report,
            RegroupReport {
                relabeled: 1,
                repacked: 1
            }
        );
        let second = plan_regroup(&h.workspaces().unwrap());
        assert!(second.renames.is_empty());
        assert!(second.moves.is_empty());
    }

    #[test]
    fn nested_generated_prefixes_are_idempotent() {
        let h = FakeHerdr::new(vec![
            parent("P"),
            child("C1", "worktree/rff/task", "P"),
            child("C2", "worktree/worktree/fix/foo", "P"),
        ]);
        let first = plan_regroup(&h.workspaces().unwrap());
        apply(&h, &first).unwrap();
        let second = plan_regroup(&h.workspaces().unwrap());
        assert!(second.renames.is_empty(), "{second:?}");
        assert!(second.moves.is_empty());
        assert_eq!(h.workspaces().unwrap()[2].label, "fix/foo");
    }

    #[test]
    fn two_groups_simulated_order() {
        let h = FakeHerdr::new(vec![
            parent("P1"),
            parent("P2"),
            child("C2", "two", "P2"),
            child("C1", "one", "P1"),
        ]);
        let plan = plan_regroup(&h.workspaces().unwrap());
        assert_eq!(
            plan.moves,
            [Move {
                workspace_ids: vec!["C1".into()],
                before: Some("P2".into())
            }]
        );
        apply(&h, &plan).unwrap();
        assert_eq!(
            h.workspaces()
                .unwrap()
                .iter()
                .map(|w| w.workspace_id.as_str())
                .collect::<Vec<_>>(),
            ["P1", "C1", "P2", "C2"]
        );
    }

    #[test]
    fn never_renames_to_empty_label() {
        let plan = plan_regroup(&[
            parent("P"),
            child("C1", "worktree/", "P"),
            child("C2", "rff/", "P"),
        ]);
        assert!(plan.renames.is_empty());
    }

    #[test]
    fn child_before_parent_moves_after_it() {
        let h = FakeHerdr::new(vec![child("C", "task", "P"), parent("P"), parent("X")]);
        let plan = plan_regroup(&h.workspaces().unwrap());
        assert_eq!(
            plan.moves,
            [Move {
                workspace_ids: vec!["C".into()],
                before: Some("X".into())
            }]
        );
        apply(&h, &plan).unwrap();
        assert_eq!(
            h.workspaces()
                .unwrap()
                .iter()
                .map(|w| w.workspace_id.as_str())
                .collect::<Vec<_>>(),
            ["P", "C", "X"]
        );
    }
}
