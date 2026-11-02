//! Ticket intake and owned worktree preparation.

use std::{path::Path, process::Command, time::Duration};

use crate::{
    error::ShepherdError,
    store::{Store, Ticket},
};

#[derive(Clone, Debug)]
pub struct IntakeRequest {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub acceptance: Vec<String>,
    pub kind: String,
    pub base: Option<String>,
}

fn slug(text: &str) -> String {
    let mut slug = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() == 40 {
            break;
        }
    }
    slug.trim_end_matches('-').into()
}

fn git(repo: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let output = crate::proc::capture(
        Command::new("git").current_dir(repo).args(args),
        Duration::from_secs(20),
        None,
    )?;
    if !output.status.success() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "git failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.trim().into())
}

pub fn integration_branch(repo: &Path, setting: &str) -> String {
    if setting != "auto" {
        return setting.into();
    }
    if git(
        repo,
        &["show-ref", "--verify", "refs/remotes/origin/development"],
    )
    .is_ok()
    {
        return "development".into();
    }
    // A fresh local repository may have no remote HEAD yet.
    git(repo, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|_| "main".into())
}

pub fn add(
    store: &Store,
    request: &IntakeRequest,
    integration_setting: &str,
    now: &str,
) -> Result<Ticket, ShepherdError> {
    if request.title.trim().is_empty() {
        return Err(ShepherdError::Usage(
            "ticket title must not be empty".into(),
        ));
    }
    if !matches!(
        request.kind.as_str(),
        "feat" | "fix" | "refactor" | "docs" | "chore"
    ) {
        return Err(ShepherdError::Usage("unsupported ticket type".into()));
    }
    let slug = slug(request.id.as_deref().unwrap_or(&request.title));
    if slug.is_empty() {
        return Err(ShepherdError::Usage(
            "ticket needs an ASCII letter or number for its slug".into(),
        ));
    }
    let id = request.id.clone().unwrap_or_else(|| slug.clone());
    let data = store.read()?;
    let project = data.project_root;
    let base = request.base.clone().unwrap_or_else(|| {
        if integration_setting != "auto" {
            return integration_setting.into();
        }
        if let Some(trust) = data.trust {
            // The default was captured before agents could change remote HEAD.
            let development = trust.common_git_dir.join("refs/remotes/origin/development");
            if development.is_file() {
                "development".into()
            } else {
                trust.default_branch
            }
        } else {
            integration_branch(&project, integration_setting)
        }
    });
    if base.trim().is_empty() || base.starts_with('-') || base.contains(['\n', '\r']) {
        return Err(ShepherdError::Usage("invalid integration branch".into()));
    }
    let mut ticket = Ticket {
        id,
        branch: format!("{}/{slug}", request.kind),
        slug,
        title: request.title.clone(),
        body: request.body.clone(),
        acceptance: request.acceptance.clone(),
        base,
        created_at: now.into(),
        updated_at: now.into(),
        ..Ticket::default()
    };
    store.update(|data| {
        if request.id.is_none() {
            // Allocate under the store lock so concurrent intake reserves unique
            // IDs. Git lists loose and packed refs, including other worktrees.
            let branches = git(
                &project,
                &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
            )?;
            let original_slug = ticket.slug.clone();
            let mut suffix = 2;
            while data
                .tickets
                .iter()
                .any(|existing| existing.id == ticket.id || existing.branch == ticket.branch)
                || branches.lines().any(|branch| branch == ticket.branch)
            {
                ticket.slug = format!("{original_slug}-{suffix}");
                ticket.id.clone_from(&ticket.slug);
                ticket.branch = format!("{}/{}", request.kind, ticket.slug);
                suffix += 1;
            }
        }
        if data.tickets.iter().any(|existing| existing.id == ticket.id) {
            return Err(ShepherdError::Usage(format!(
                "duplicate ticket id: {}",
                ticket.id
            )));
        }
        if data
            .tickets
            .iter()
            .any(|existing| existing.branch == ticket.branch)
        {
            return Err(ShepherdError::Usage(format!(
                "ticket branch already reserved: {}",
                ticket.branch
            )));
        }
        data.tickets.push(ticket.clone());
        Ok(ticket)
    })
}

pub fn cancel(
    store: &Store,
    id: &str,
    now: &str,
    max_revision_rounds: u32,
) -> Result<Ticket, ShepherdError> {
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let _operation = super::operation::exclusive(store, id)?;
    close_ticket(store, id, now, max_revision_rounds)
}

/// Retire every admitted agent before closing. Keep admission excluded through
/// the snapshot, external retirement, and terminal state transaction.
pub fn cancel_agents(
    h: &dyn crate::herdr::Herdr,
    store: &Store,
    id: &str,
    now: &str,
    max_revision_rounds: u32,
) -> Result<Ticket, ShepherdError> {
    cancel_agents_inner(h, store, id, now, max_revision_rounds, None)
}

pub fn cancel_agents_with_config(
    h: &dyn crate::herdr::Herdr,
    store: &Store,
    id: &str,
    now: &str,
    max_revision_rounds: u32,
    cfg: &crate::config::Config,
) -> Result<Ticket, ShepherdError> {
    cancel_agents_inner(h, store, id, now, max_revision_rounds, Some(cfg))
}

fn cancel_agents_inner(
    h: &dyn crate::herdr::Herdr,
    store: &Store,
    id: &str,
    now: &str,
    max_revision_rounds: u32,
    cfg: Option<&crate::config::Config>,
) -> Result<Ticket, ShepherdError> {
    let _hooks = crate::hooks::defer();
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let _operation = super::operation::exclusive(store, id)?;
    for agent in store.read()?.agents.into_iter().filter(|agent| {
        agent.ticket == id
            && !matches!(
                agent.status,
                crate::store::AgentStatus::Retired | crate::store::AgentStatus::LaunchFailed
            )
    }) {
        if let Some(cfg) = cfg {
            crate::flock::cancel_with_config(h, store, &agent.name, false, cfg)?;
        } else {
            crate::flock::cancel(h, store, &agent.name, false)?;
        }
    }
    close_ticket(store, id, now, max_revision_rounds)
}

fn close_ticket(
    store: &Store,
    id: &str,
    now: &str,
    max_revision_rounds: u32,
) -> Result<Ticket, ShepherdError> {
    store.update(|data| {
        if data.agents.iter().any(|agent| {
            agent.ticket == id
                && !matches!(
                    agent.status,
                    crate::store::AgentStatus::Retired | crate::store::AgentStatus::LaunchFailed
                )
        }) {
            return Err(ShepherdError::Usage(
                "retire ticket agents before closing".into(),
            ));
        }

        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        super::state::transition_with_limit(
            ticket,
            crate::store::TicketState::Closed,
            None,
            now,
            max_revision_rounds,
        )?;
        Ok(ticket.clone())
    })
}

pub struct StartTicketOptions<'a> {
    pub state_dir: &'a Path,
    pub config_dir: &'a Path,
    pub bin: &'a Path,
    pub settings: &'a crate::roles::ShepherdSettings,
    pub now: &'a str,
}

/// Reserve a ticket slot and prepare its owned checkout. The orchestration
/// caller launches the planner with the returned ticket and its role prompt.
pub fn start_ticket(
    h: &dyn crate::herdr::Herdr,
    store: &Store,
    id: &str,
    options: &StartTicketOptions<'_>,
) -> Result<Ticket, ShepherdError> {
    start_ticket_with_backend(h, &crate::github::gh::GhBackend, store, id, options, true)
}

/// Start with an injectable read-only GitHub backend for protection warnings.
pub fn start_ticket_with_backend(
    h: &dyn crate::herdr::Herdr,
    gh: &dyn crate::github::PrBackend,
    store: &Store,
    id: &str,
    options: &StartTicketOptions<'_>,
    notifications: bool,
) -> Result<Ticket, ShepherdError> {
    use crate::store::{BlockedReason, TicketState};

    let _operation = super::operation::shared(store, id)?;
    let preflight = store.read()?;
    let at_capacity = |data: &crate::store::StoreData| {
        data.agents
            .iter()
            .filter(|agent| {
                agent.role != "lead"
                    && matches!(
                        agent.status,
                        crate::store::AgentStatus::Starting
                            | crate::store::AgentStatus::Working
                            | crate::store::AgentStatus::Blocked
                    )
            })
            .count()
            >= options.settings.max_parallel_agents
    };
    if at_capacity(&preflight) {
        return Err(ShepherdError::Capacity);
    }
    if !super::scheduler::next_actions(&preflight, options.settings)
        .contains(&super::scheduler::Action::StartTicket(id.into()))
    {
        return Err(ShepherdError::Usage(format!(
            "ticket {id} is not queued with an available scheduling slot"
        )));
    }
    crate::trusted::ensure_snapshot(options.state_dir, &preflight.project_root)?;
    let (project, ticket) = store.update(|data| {
        if at_capacity(data) {
            return Err(ShepherdError::Capacity);
        }
        if !super::scheduler::next_actions(data, options.settings)
            .contains(&super::scheduler::Action::StartTicket(id.into()))
        {
            return Err(ShepherdError::Usage(format!(
                "ticket {id} is not queued with an available scheduling slot"
            )));
        }
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        super::state::transition_with_limit(
            ticket,
            TicketState::Planning,
            None,
            options.now,
            options.settings.max_revision_rounds,
        )?;
        Ok((data.project_root.clone(), ticket.clone()))
    })?;

    let setup = || -> Result<Ticket, ShepherdError> {
        let destination = options
            .state_dir
            .join("worktrees")
            .join(crate::store::project_hash(&project))
            .join(ticket.branch.replace('/', "--"));
        let destination = if destination.is_absolute() {
            destination
        } else {
            std::env::current_dir()
                .map_err(anyhow::Error::from)?
                .join(destination)
        };
        std::fs::create_dir_all(destination.parent().expect("worktree has parent"))
            .map_err(anyhow::Error::from)?;
        let remote_base = format!("origin/{}", ticket.base);
        let base = if git(
            &project,
            &[
                "show-ref",
                "--verify",
                &format!("refs/heads/{}", ticket.base),
            ],
        )
        .is_err()
            && git(
                &project,
                &[
                    "show-ref",
                    "--verify",
                    &format!("refs/remotes/{remote_base}"),
                ],
            )
            .is_ok()
        {
            &remote_base
        } else {
            &ticket.base
        };
        let workspace =
            h.create_worktree(&project, &ticket.branch, &ticket.slug, base, &destination)?;
        let path = workspace
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.checkout_path.clone())
            .ok_or_else(|| anyhow::anyhow!("created workspace has no checkout path"))?;
        if path.canonicalize().map_err(anyhow::Error::from)?
            != destination.canonicalize().map_err(anyhow::Error::from)?
        {
            return Err(anyhow::anyhow!("created worktree differs from reserved path").into());
        }
        // Save resource ownership before hook or dependency setup can fail.
        store.update(|data| {
            let ticket = data
                .tickets
                .iter_mut()
                .find(|ticket| ticket.id == id)
                .unwrap();
            ticket.worktree = Some(path.clone());
            ticket.workspace_id = Some(workspace.workspace_id.clone());
            Ok(())
        })?;
        crate::gate::hook::install_configured(
            &path,
            &ticket.branch,
            options.state_dir,
            options.bin,
            options.config_dir,
        )?;
        if project.join("node_modules").is_dir() && !path.join("node_modules").exists() {
            let output = crate::proc::capture(
                Command::new("cp")
                    .args(["-a", "--reflink=auto"])
                    .arg(project.join("node_modules"))
                    .arg(&path),
                Duration::from_secs(120),
                None,
            )?;
            if !output.status.success() {
                return Err(anyhow::anyhow!("seed node_modules: {}", output.stderr.trim()).into());
            }
        }
        store
            .read()?
            .tickets
            .into_iter()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))
    };
    let result = setup();
    if result.is_ok() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        if let Err(error) =
            crate::github::protection::check_started_ticket(store, gh, h, id, now, notifications)
        {
            eprintln!("shepherd: branch protection warning: {error}");
        }
        return store
            .read()?
            .tickets
            .into_iter()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")));
    }
    if result.is_err() {
        store.update(|data| {
            let ticket = data
                .tickets
                .iter_mut()
                .find(|ticket| ticket.id == id)
                .unwrap();
            if ticket.state == TicketState::Planning {
                super::state::transition_with_limit(
                    ticket,
                    TicketState::Blocked,
                    Some(BlockedReason::LaunchFailed),
                    options.now,
                    options.settings.max_revision_rounds,
                )?;
            }
            Ok(())
        })?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args([
                "-c",
                "user.name=Shepherd Test",
                "-c",
                "user.email=shepherd@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[test]
    fn slug_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path(), dir.path()).unwrap();
        let request = IntakeRequest {
            id: Some("SYM-5567".into()),
            title: "A small fix".into(),
            body: "Keep the text".into(),
            acceptance: vec!["The test passes".into()],
            kind: "fix".into(),
            base: Some("develop".into()),
        };
        let ticket = add(&store, &request, "auto", "0001").unwrap();
        assert_eq!(ticket.id, "SYM-5567");
        assert_eq!(ticket.slug, "sym-5567");
        assert_eq!(ticket.branch, "fix/sym-5567");
        assert_eq!(ticket.base, "develop");
        assert_eq!(ticket.state, crate::store::TicketState::Queued);
        assert_eq!(ticket.acceptance, ["The test passes"]);
        let error = add(&store, &request, "auto", "0002").unwrap_err();
        assert!(matches!(error, ShepherdError::Usage(_)));
        assert_eq!(error.to_string(), "duplicate ticket id: SYM-5567");
        assert_eq!(store.read().unwrap().tickets.len(), 1);
    }

    fn adhoc_request() -> IntakeRequest {
        IntakeRequest {
            id: None,
            title: "Fix login banner".into(),
            body: "Fix the typo in the login banner.\nKeep its styling.".into(),
            acceptance: vec!["The login banner uses the corrected text".into()],
            kind: "fix".into(),
            base: Some("main".into()),
        }
    }

    #[test]
    fn adhoc_collisions_suffix_id_and_branch_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch", "main"]);
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let request = adhoc_request();
        let first = add(&store, &request, "auto", "0001").unwrap();
        assert_eq!(first.id, "fix-login-banner");
        assert_eq!(first.branch, "fix/fix-login-banner");
        for suffix in [2, 3] {
            let mut next = request.clone();
            next.body = format!("New request {suffix}");
            let ticket = add(&store, &next, "auto", "0002").unwrap();
            assert_eq!(ticket.id, format!("fix-login-banner-{suffix}"));
            assert_eq!(ticket.slug, ticket.id);
            assert_eq!(ticket.branch, format!("fix/fix-login-banner-{suffix}"));
            assert_eq!(ticket.body, next.body);
        }
        let tickets = store.read().unwrap().tickets;
        assert_eq!(tickets.len(), 3);
        assert_eq!(tickets[0], first);
    }

    #[test]
    fn adhoc_collisions_skip_local_branches_and_reserved_branches() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch", "main"]);
        git(dir.path(), &["commit", "--allow-empty", "-m", "initial"]);
        git(dir.path(), &["branch", "fix/fix-login-banner"]);
        git(dir.path(), &["branch", "fix/fix-login-banner-2"]);
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    branch: "fix/fix-login-banner-3".into(),
                    ..Default::default()
                });
                data.tickets.push(Ticket {
                    id: "fix-login-banner-4".into(),
                    branch: "docs/other-branch".into(),
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let ticket = add(&store, &adhoc_request(), "auto", "now").unwrap();
        assert_eq!(ticket.id, "fix-login-banner-5");
        assert_eq!(ticket.slug, "fix-login-banner-5");
        assert_eq!(ticket.branch, "fix/fix-login-banner-5");
        assert_eq!(
            git(dir.path(), &["branch", "--list", "fix/fix-login-banner-5"]),
            ""
        );
    }

    #[test]
    fn concurrent_adhoc_intake_reserves_distinct_ids() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch", "main"]);
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let barrier = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let store = &store;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    add(store, &adhoc_request(), "auto", "now").unwrap();
                });
            }
        });
        let tickets = store.read().unwrap().tickets;
        let ids: std::collections::HashSet<_> = tickets.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.len(), 4);
        assert!(ids.contains("fix-login-banner"));
        assert!(ids.contains("fix-login-banner-4"));
    }

    #[test]
    fn integration_branch_auto() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--initial-branch", "trunk"]);
        git(dir.path(), &["commit", "--allow-empty", "-m", "initial"]);
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/trunk", "HEAD"],
        );
        git(
            dir.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        assert_eq!(integration_branch(dir.path(), "auto"), "trunk");
        git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/development", "HEAD"],
        );
        assert_eq!(integration_branch(dir.path(), "auto"), "development");
        assert_eq!(integration_branch(dir.path(), "staging"), "staging");
    }

    #[test]
    fn cancel_keeps_worktree_and_records_transition() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path(), dir.path()).unwrap();
        store
            .update(|data| {
                data.tickets.push(crate::store::Ticket {
                    id: "T1".into(),
                    state: crate::store::TicketState::Building,
                    worktree: Some(dir.path().join("ticket")),
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let ticket = cancel(&store, "T1", "later", 1).unwrap();
        assert_eq!(ticket.state, crate::store::TicketState::Closed);
        assert_eq!(ticket.worktree, Some(dir.path().join("ticket")));
        assert_eq!(ticket.history.len(), 1);
        assert_eq!(ticket.updated_at, "later");
    }

    #[test]
    fn start_ticket_records_owned_worktree_and_installs_hook() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "--initial-branch", "trunk"]);
        git(&root, &["commit", "--allow-empty", "-m", "initial"]);
        let state_dir = dir.path().join("state");
        let store = crate::store::Store::open(&state_dir, &root).unwrap();
        let request = IntakeRequest {
            id: Some("T1".into()),
            title: "Tiny task".into(),
            body: String::new(),
            acceptance: vec![],
            kind: "feat".into(),
            base: Some("trunk".into()),
        };
        let ticket = add(&store, &request, "auto", "before").unwrap();
        let path = state_dir
            .join("worktrees")
            .join(crate::store::project_hash(&root))
            .join("feat--t1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // FakeHerdr supplies the workspace API response; real git supplies the checkout.
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                &ticket.branch,
                path.to_str().unwrap(),
                "trunk",
            ],
        );
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/fixture.txt"), "fixture").unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        let settings = crate::roles::ShepherdSettings::default();
        let bin = std::env::current_exe().unwrap();
        let config_dir = dir.path().join("selected-config");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("shepherd.toml"),
            "[shepherd]\nprotected_branches=['feat/t1']\n",
        )
        .unwrap();
        let options = StartTicketOptions {
            state_dir: &state_dir,
            config_dir: &config_dir,
            bin: &bin,
            settings: &settings,
            now: "after",
        };
        let backend = crate::github::fake::FakeBackend::new(vec![]);
        let started =
            start_ticket_with_backend(&h, &backend, &store, "T1", &options, true).unwrap();
        assert_eq!(started.state, crate::store::TicketState::Planning);
        assert_eq!(started.worktree, Some(path.clone()));
        assert_eq!(started.workspace_id.as_deref(), Some("fake-w1"));
        assert!(!started.protection_warnings.is_empty());
        assert!(
            started
                .protection_warnings
                .iter()
                .all(|warning| warning.reason.starts_with("unknown:"))
        );
        assert_eq!(
            store.read().unwrap().tickets[0].protection_warnings,
            started.protection_warnings
        );
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("notify ")
                    && call.contains("branch protection unknown:"))
        );
        assert_eq!(
            git(
                &path,
                &["config", "--worktree", "--get", "shepherd.ticketBranch"]
            ),
            "feat/t1"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("node_modules/fixture.txt")).unwrap(),
            "fixture"
        );
        assert!(start_ticket_with_backend(&h, &backend, &store, "T1", &options, true).is_err());
        assert_eq!(h.workspaces.borrow().len(), 1);
        let hook = std::fs::read_to_string(state_dir.join("hooks/pre-push")).unwrap();
        assert!(hook.contains(&format!(
            "--config-dir {}",
            crate::gate::shim::shell_quote(&config_dir)
        )));
    }
}
