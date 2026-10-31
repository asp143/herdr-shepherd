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

fn handle_event_inner(
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

pub fn handle_event(
    name: &str,
    payload: &serde_json::Value,
    cfg: &Config,
    h: &dyn Herdr,
) -> Result<EventOutcome, ShepherdError> {
    let state = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| ".".into())
                .join(".local/state/herdr/plugins/herdr-shepherd")
        });
    handle_event_in_state(name, payload, cfg, h, &state)
}

pub fn handle_event_in_state(
    name: &str,
    payload: &serde_json::Value,
    cfg: &Config,
    h: &dyn Herdr,
    state_dir: &std::path::Path,
) -> Result<EventOutcome, ShepherdError> {
    if !matches!(name, "worktree.created" | "worktree.removed") || !cfg.upkeep.regroup {
        return handle_event_inner(name, payload, cfg, h);
    }
    std::fs::create_dir_all(state_dir).map_err(anyhow::Error::from)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(state_dir.join("regroup.lock"))
        .map_err(anyhow::Error::from)?;
    fs2::FileExt::lock_exclusive(&lock).map_err(anyhow::Error::from)?;
    handle_event_inner(name, payload, cfg, h)
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
        let outcome = handle_event_in_state(
            "worktree.created",
            &serde_json::json!({}),
            &cfg,
            &h,
            config_dir.path(),
        )
        .unwrap();
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
            handle_event_in_state(
                "worktree.created",
                &serde_json::json!({}),
                &cfg,
                &h,
                config_dir.path()
            )
            .unwrap(),
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
            handle_event_in_state(
                "pane.created",
                &serde_json::json!({}),
                &cfg,
                &h,
                config_dir.path()
            )
            .unwrap(),
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
            handle_event_in_state(
                "worktree.removed",
                &serde_json::json!({}),
                &cfg,
                &h,
                config_dir.path()
            )
            .unwrap(),
            EventOutcome::Regrouped(RegroupReport { repacked: 1, .. })
        ));
    }
    #[test]
    fn concurrent_events_serialize_snapshot_and_apply() {
        use crate::herdr::{AgentInfo, Workspace};
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        struct Tracking<'a> {
            fake: FakeHerdr,
            active: &'a AtomicUsize,
            max: &'a AtomicUsize,
        }
        impl Herdr for Tracking<'_> {
            fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
                let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(80));
                self.fake.workspaces()
            }
            fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
                self.fake.agents()
            }
            fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
                self.fake.rename_workspace(id, label)
            }
            fn move_block(
                &self,
                ids: &[String],
                before: Option<&str>,
            ) -> Result<(), ShepherdError> {
                let result = self.fake.move_block(ids, before);
                self.active.fetch_sub(1, Ordering::SeqCst);
                result
            }
        }
        let config_dir = tempfile::tempdir().unwrap();
        let cfg = config::load(config_dir.path(), None).unwrap();
        let state = tempfile::tempdir().unwrap();
        let active = AtomicUsize::new(0);
        let max = AtomicUsize::new(0);
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let cfg = &cfg;
                let state = state.path();
                let active = &active;
                let max = &max;
                let barrier = &barrier;
                scope.spawn(move || {
                    let h = Tracking {
                        fake: unpacked(),
                        active,
                        max,
                    };
                    barrier.wait();
                    handle_event_in_state(
                        "worktree.created",
                        &serde_json::json!({}),
                        cfg,
                        &h,
                        state,
                    )
                    .unwrap();
                });
            }
        });
        assert_eq!(max.load(Ordering::SeqCst), 1);
    }
}
