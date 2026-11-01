//! Ticket layout contract through dispatch, CLI spawn and revision APIs.
use super::*;
use herdr_shepherd::{
    config,
    herdr::{
        fake::FakeHerdr,
        model::{Workspace, WorktreeInfo},
    },
    store::Ticket,
    ticket::intake::IntakeRequest,
};
use std::{fs, path::PathBuf, process::Command};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    config_dir: PathBuf,
    store: Store,
    cfg: Config,
    h: FakeHerdr,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Fixture"]);
        git(&root, &["config", "user.email", "fixture@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-m", "base"]);
        let origin = dir.path().join("origin.git");
        git(&root, &["init", "--bare", origin.to_str().unwrap()]);
        git(
            &root,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&root, &["push", "origin", "main"]);
        let state = dir.path().join("state");
        let config_dir = dir.path().join("config");
        fs::create_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join("shepherd.toml"),
            "[shepherd]\nmax_parallel_agents=16\n[upkeep]\nnotifications=false\n",
        )
        .unwrap();
        let cfg = config::load(&config_dir, None).unwrap();
        let store = Store::open(&state, &root).unwrap();
        herdr_shepherd::trusted::ensure_snapshot(&state, &root).unwrap();
        intake::add(
            &store,
            &IntakeRequest {
                id: Some("T-1".into()),
                title: "Layout ticket".into(),
                body: "Build it".into(),
                acceptance: vec![],
                kind: "feat".into(),
                base: Some("main".into()),
            },
            "main",
            "0",
        )
        .unwrap();
        let h = FakeHerdr::new(vec![
            Workspace {
                workspace_id: "repo".into(),
                label: "project".into(),
                worktree: Some(WorktreeInfo {
                    repo_key: root.display().to_string(),
                    repo_root: Some(root.clone()),
                    checkout_path: Some(root.clone()),
                    is_linked_worktree: false,
                }),
            },
            Workspace {
                workspace_id: "unrelated".into(),
                label: "notes".into(),
                worktree: None,
            },
        ]);
        h.git_worktrees.set(true);
        Self {
            _dir: dir,
            root,
            state,
            config_dir,
            store,
            cfg,
            h,
        }
    }
    fn dispatch(&self) -> serde_json::Value {
        dispatch(
            &self.h,
            &self.store,
            &self.cfg,
            &self.state,
            &self.config_dir,
        )
        .unwrap()
    }
    fn ticket(&self) -> Ticket {
        self.store.read().unwrap().tickets[0].clone()
    }
    fn ids(&self) -> Vec<String> {
        self.h
            .workspaces
            .borrow()
            .iter()
            .map(|w| w.workspace_id.clone())
            .collect()
    }
}

#[test]
fn dispatch_groups_one_ticket_workspace_without_an_event() {
    let f = Fixture::new();
    assert_eq!(f.dispatch()["started"].as_array().unwrap().len(), 1);
    let ticket = f.ticket();
    let path = ticket.worktree.as_ref().unwrap();
    assert_eq!(ticket.branch, "feat/t-1");
    assert_eq!(git(path, &["branch", "--show-current"]), "feat/t-1");
    assert_eq!(f.ids(), ["repo", "fake-w3", "unrelated"]);
    assert_eq!(f.h.workspaces.borrow()[1].label, ticket.slug);
    assert_eq!(
        f.h.workspaces.borrow()[1]
            .worktree
            .as_ref()
            .unwrap()
            .checkout_path
            .as_ref(),
        Some(path)
    );
    assert_eq!(
        f.h.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("create_worktree "))
            .cloned()
            .collect::<Vec<_>>(),
        [format!(
            "create_worktree {} feat/t-1 {} main {}",
            f.root.display(),
            ticket.slug,
            path.display()
        )]
    );
    assert!(
        f.h.calls
            .borrow()
            .contains(&"move [\"fake-w3\"] Some(\"unrelated\")".into())
    );
    assert_eq!(f.store.read().unwrap().agents[0].role, "planner");
}

#[test]
fn dispatch_honors_disabled_regroup_and_retains_ticket_on_move_failure() {
    for disabled in [true, false] {
        let mut f = Fixture::new();
        f.cfg.upkeep.regroup = !disabled;
        f.h.move_error.set(!disabled);
        assert_eq!(f.dispatch()["started"].as_array().unwrap().len(), 1);
        assert_eq!(f.ids(), ["repo", "unrelated", "fake-w3"]);
        assert_eq!(f.ticket().state, TicketState::Planning);
        assert!(f.ticket().worktree.unwrap().exists());
        let moves =
            f.h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("move "))
                .count();
        assert_eq!(moves, usize::from(!disabled));
    }
}

#[test]
fn every_role_and_unit_uses_ticket_tabs_and_integration_retires_only_children() {
    let f = Fixture::new();
    f.dispatch();
    let task = f.root.parent().unwrap().join("task.txt");
    fs::write(&task, "Implement this unit").unwrap();
    for (role, unit) in [
        ("researcher", None),
        ("coder", None),
        ("reviewer", None),
        ("qa", None),
        ("coder", Some(1)),
        ("coder", Some(2)),
    ] {
        crate::cli_flock::run(
            &crate::cli_flock::AgentCommand::Spawn {
                ticket: "T-1".into(),
                role: role.into(),
                task_file: task.clone(),
                unit,
            },
            &f.h,
            &f.root,
            &f.state,
            &f.config_dir,
            true,
        )
        .unwrap();
    }
    let data = f.store.read().unwrap();
    let ticket = &data.tickets[0];
    let tip = git(ticket.worktree.as_ref().unwrap(), &["rev-parse", "HEAD"]);
    assert_eq!(data.agents.len(), 7);
    assert_eq!(ticket.children.len(), 2);
    assert_ne!(ticket.children[0].path, ticket.children[1].path);
    let mut expected_tabs = Vec::new();
    for (index, agent) in data.agents.iter().enumerate() {
        let label = match index {
            5 => "coder-1",
            6 => "coder-2",
            _ => &agent.role,
        };
        expected_tabs.push(format!(
            "create_tab fake-w3 {label} {}",
            agent.worktree.display()
        ));
        let info = f.h.agent_get(&agent.name).unwrap().unwrap();
        assert_eq!(info.workspace_id.as_deref(), Some("fake-w3"));
        assert_eq!(info.cwd.as_ref(), Some(&agent.worktree));
        if index < 5 {
            assert_eq!(Some(&agent.worktree), ticket.worktree.as_ref());
        }
    }
    for child in &ticket.children {
        assert_eq!(child.branch, format!("feat/t-1--{}", child.unit));
        assert_eq!(
            git(&child.path, &["branch", "--show-current"]),
            child.branch
        );
        assert_eq!(child.base_sha, tip);
        assert_eq!(git(&child.path, &["rev-parse", "HEAD"]), tip);
        assert!(
            serde_json::to_value(child)
                .unwrap()
                .get("workspace_id")
                .is_none()
        );
        assert_eq!(
            git(
                &child.path,
                &["config", "--worktree", "--get", "shepherd.ticketBranch"]
            ),
            child.branch
        );
        let hook = git(
            &child.path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "hooks/pre-push",
            ],
        );
        let script = fs::read_to_string(hook).unwrap();
        assert!(script.contains("shepherd"));
        assert!(script.contains(&herdr_shepherd::gate::shim::shell_quote(&f.config_dir)));
        assert_eq!(
            data.agents
                .iter()
                .filter(|a| a.worktree == child.path)
                .count(),
            1
        );
        let file = format!("unit-{}.txt", child.unit);
        fs::write(child.path.join(&file), format!("unit {}", child.unit)).unwrap();
        git(&child.path, &["add", &file]);
        git(
            &child.path,
            &["commit", "-m", &format!("unit {}", child.unit)],
        );
    }
    assert_eq!(
        f.h.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("create_tab "))
            .cloned()
            .collect::<Vec<_>>(),
        expected_tabs
    );
    assert_eq!(f.ids(), ["repo", "fake-w3", "unrelated"]);
    // End-to-end ownership uses actual AgentInfo with the shared workspace ID.
    // All agents reported; only the two child tabs may be retired by integration.
    f.store
        .update(|data| {
            for agent in &mut data.agents {
                agent.status = AgentStatus::Reported;
            }
            Ok(())
        })
        .unwrap();
    let report = integrate(&f.h, &f.store, &f.cfg, "T-1").unwrap();
    assert_eq!(report.units, [1, 2]);
    let after = f.store.read().unwrap();
    assert!(after.tickets[0].children.is_empty());
    for (index, agent) in after.agents.iter().enumerate() {
        assert_eq!(
            agent.status,
            if index < 5 {
                AgentStatus::Reported
            } else {
                AgentStatus::Retired
            }
        );
        assert_eq!(f.h.agent_get(&agent.name).unwrap().is_some(), index < 5);
    }
    for child in &ticket.children {
        assert!(!child.path.exists());
        assert!(git(&f.root, &["branch", "--list", &child.branch]).is_empty());
        assert!(
            ticket
                .worktree
                .as_ref()
                .unwrap()
                .join(format!("unit-{}.txt", child.unit))
                .exists()
        );
    }
    let closes: Vec<_> =
        f.h.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("close_tab "))
            .cloned()
            .collect();
    assert_eq!(
        closes,
        data.agents[5..]
            .iter()
            .map(|a| format!("close_tab {}", a.tab_id.as_ref().unwrap()))
            .collect::<Vec<_>>()
    );
    assert_eq!(f.h.agent_records.borrow().len(), 5);
    assert_eq!(f.h.pane_cwds.borrow().len(), 5);
    assert_eq!(f.h.pane_workspaces.borrow().len(), 5);
    assert_eq!(f.ids(), ["repo", "fake-w3", "unrelated"]);
    let resources: Vec<_> =
        f.h.calls
            .borrow()
            .iter()
            .filter(|call| {
                [
                    "create_worktree ",
                    "create_tab ",
                    "move ",
                    "rename ",
                    "remove_worktree ",
                    "close_workspace ",
                    "close_tab ",
                ]
                .iter()
                .any(|prefix| call.starts_with(prefix))
            })
            .cloned()
            .collect();
    let mut expected = vec![
        format!(
            "create_worktree {} feat/t-1 {} main {}",
            f.root.display(),
            ticket.slug,
            ticket.worktree.as_ref().unwrap().display()
        ),
        "move [\"fake-w3\"] Some(\"unrelated\")".into(),
    ];
    expected.extend(expected_tabs);
    expected.extend(closes);
    assert_eq!(resources, expected, "complete layout resource call history");
}

#[test]
fn revision_stages_keep_the_dispatched_ticket_workspace() {
    use herdr_shepherd::{
        gate::{BranchPolicy, Gate},
        github::fake::FakeBackend,
        ticket::revise::{RevisionOutcome, RevisionRun, RevisionStage, resume_revision},
    };
    let f = Fixture::new();
    f.dispatch();
    let ticket = f.ticket();
    let head = git(ticket.worktree.as_ref().unwrap(), &["rev-parse", "HEAD"]);
    let gate = Gate {
        policy: BranchPolicy::new(&[], Some("main"), Some("main")).unwrap(),
        ticket_branch: ticket.branch.clone(),
        worktree: ticket.worktree.clone().unwrap(),
        state_dir: f.state.clone(),
        config_dir: f.config_dir.clone(),
    };
    let gh = FakeBackend::new(vec![]);
    for (stage, role) in [
        (RevisionStage::Coder, "coder"),
        (RevisionStage::Reviewer, "reviewer"),
        (RevisionStage::Qa, "qa"),
    ] {
        f.store
            .update(|data| {
                for agent in &mut data.agents {
                    agent.status = AgentStatus::Reported;
                }
                let ticket = &mut data.tickets[0];
                ticket.state = TicketState::Revising;
                ticket.revision_task = Some("Apply review feedback".into());
                ticket.revision_run = Some(RevisionRun {
                    base_sha: head.clone(),
                    synced_sha: Some(head.clone()),
                    stage,
                    agent: None,
                    tested_sha: None,
                });
                Ok(())
            })
            .unwrap();
        let outcome = resume_revision(&f.h, &gh, &f.store, &f.cfg, &gate, "T-1", "1").unwrap();
        assert!(matches!(outcome, RevisionOutcome::Started { role: actual, .. } if actual == role));
    }
    assert!(
        gh.calls.borrow().is_empty(),
        "no network or publication path"
    );
    let tabs: Vec<_> =
        f.h.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("create_tab "))
            .cloned()
            .collect();
    let path = ticket.worktree.unwrap();
    assert_eq!(
        tabs,
        [
            "planner",
            "revision coder",
            "revision reviewer",
            "revision qa"
        ]
        .map(|label| format!("create_tab fake-w3 {label} {}", path.display()))
    );
    assert_eq!(f.ids(), ["repo", "fake-w3", "unrelated"]);
    assert_eq!(
        f.h.calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("create_worktree "))
            .count(),
        1
    );
    for info in f.h.agent_records.borrow().iter() {
        assert_eq!(info.workspace_id.as_deref(), Some("fake-w3"));
        assert_eq!(info.cwd.as_ref(), Some(&path));
    }
}
