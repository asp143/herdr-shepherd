//! Poll PR lifecycle state for tickets in trusted project stores.
use std::path::Path;

use crate::{
    config::Config,
    error::ShepherdError,
    github::{Checks, PrBackend, PrState},
    herdr::Herdr,
    store::{Store, Ticket, TicketState},
};

pub fn tick_tickets(
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    cfg: &Config,
    state: &Path,
    now: u64,
) -> super::TickReport {
    tick_with_progress(h, gh, cfg, state, now, &mut super::WatchState::default())
}

pub(super) fn tick_with_progress(
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    cfg: &Config,
    state: &Path,
    now: u64,
    progress: &mut super::WatchState,
) -> super::TickReport {
    let mut report = super::TickReport::default();
    if !cfg.enabled {
        return report;
    }
    let stores = match crate::supervise::project_stores(state) {
        Ok((stores, errors)) => {
            report.errors.extend(errors);
            stores
        }
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    let mut pending = Vec::new();
    for store in stores {
        let result = (|| -> Result<(), ShepherdError> {
            let data = store.read()?;
            let selected = crate::supervise::config_for(cfg, &data.project_root, state)?;
            if !selected.enabled {
                return Ok(());
            }
            let project = crate::store::project_hash(&data.project_root);
            let tickets: Vec<_> = data
                .tickets
                .into_iter()
                .filter(|ticket| {
                    ticket.pr_number.is_some()
                        && !matches!(ticket.state, TicketState::CleanedUp | TicketState::Failed)
                })
                .collect();
            pending.push((store, selected, project, tickets));
            Ok(())
        })();
        if let Err(error) = result {
            report.errors.push(error.to_string());
        }
    }
    // Flatten across projects: rotating within each store still starves later
    // projects when an earlier project's provider never finishes.
    let mut work: Vec<_> = pending
        .iter()
        .flat_map(|(store, cfg, project, tickets)| {
            tickets
                .iter()
                .map(move |ticket| (format!("{project}:{}", ticket.id), store, cfg, ticket))
        })
        .collect();
    super::resume_after(&mut work, progress.ticket_cursor.as_deref(), |item| {
        item.0.clone()
    });
    for (key, store, selected, ticket) in work {
        progress.ticket_cursor = Some(key);
        if let Err(error) = progress.checkpoint() {
            report.errors.push(error.to_string());
            break;
        }
        let result = poll_ticket(h, gh, store, selected, ticket, now);
        report.polled += 1;
        match result {
            Ok(changed) => report.updated += usize::from(changed),
            Err(error) => report.errors.push(format!("{}: {error}", ticket.id)),
        }
    }
    report
}

fn poll_ticket(
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    store: &Store,
    cfg: &Config,
    ticket: &Ticket,
    now: u64,
) -> Result<bool, ShepherdError> {
    let path = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    let Some(status) = gh.view(path, &ticket.branch)? else {
        return Ok(false);
    };
    if ticket.pr_number != Some(status.number)
        || ticket.branch != status.head
        || ticket.base != status.base
    {
        return Err(ShepherdError::GithubUnavailable(
            "PR does not match stored ticket".into(),
        ));
    }
    if status.state != PrState::Open {
        return crate::ticket::merge::handle_pr_state(
            store,
            gh,
            h,
            cfg,
            &ticket.id,
            &status,
            &now.to_string(),
        )
        .map(|outcome| outcome != crate::ticket::merge::MergeOutcome::Unchanged);
    }
    // Comment transport and payload failures must not hold up CI state changes.
    // This scope also releases lifecycle locks before pickup acquires them.
    let ci_result = (|| -> Result<bool, ShepherdError> {
        let checks = if ticket
            .ci_head_sha
            .as_ref()
            .is_some_and(|sha| status.head_oid.as_ref() == Some(sha))
        {
            status.checks
        } else {
            Checks::Pending
        };
        if ticket.state == TicketState::CiWait && checks == Checks::Failing {
            let checks = gh.failed_checks(path, status.number)?;
            let details = checks
                .iter()
                .map(|check| format!("- {}: {}", check.name, check.url))
                .collect::<Vec<_>>()
                .join("\n");
            let task = format!(
                "Fix failing CI for {} ({}). Reproduce the failures before changing code, run the failing checks, and send the complete diff through reviewer and QA before pushing the existing PR.\n{}",
                ticket.id, status.url, details,
            );
            let cap = crate::roles::resolve(cfg)?.shepherd.max_revision_rounds;
            let changed = {
                let _dispatch = crate::ticket::revise::dispatch_lock(store, &ticket.id)?;
                store.update(|data| {
                    let Some(current) = data
                        .tickets
                        .iter_mut()
                        .find(|current| same_ci_generation(current, ticket))
                    else {
                        return Ok(false);
                    };
                    crate::ticket::state::transition_with_limit(
                        current,
                        TicketState::Building,
                        None,
                        &now.to_string(),
                        cap,
                    )?;
                    current.ci_none_since = None;
                    if current.state == TicketState::Building {
                        current.ci_failure_task = Some(task.clone());
                    }
                    Ok(true)
                })?
            };
            if changed {
                let state = store
                    .read()?
                    .tickets
                    .iter()
                    .find(|t| t.id == ticket.id)
                    .map(|t| t.state);
                if state == Some(TicketState::Blocked) {
                    crate::notify::Notifier::new(h, cfg.upkeep.notifications).send(
                        &format!("{}: CI revision loop cap reached", ticket.id),
                        &status.url,
                    );
                    crate::hooks::emit(
                        cfg,
                        "ticket.blocked",
                        &serde_json::json!({"ticket":ticket.id,"reason":"loop_cap"}),
                    );
                } else {
                    crate::flock::ping_notice(
                        h,
                        store,
                        &format!(
                            "{}: CI failed; run ticket dispatch to start the fix and complete reviewer and QA",
                            ticket.id
                        ),
                        Some(&ticket.id),
                        None,
                    );
                }
                crate::hooks::emit(
                    cfg,
                    "ticket.stage_changed",
                    &serde_json::json!({"ticket":ticket.id,"from":"ci_wait","to":state}),
                );
            }
            return Ok(changed);
        }
        let no_checks_ready = if ticket.state == TicketState::CiWait {
            store.update(|data| {
                let Some(current) = data
                    .tickets
                    .iter_mut()
                    .find(|current| same_ci_generation(current, ticket))
                else {
                    return Ok(false);
                };
                if checks == Checks::None {
                    let since = current
                        .ci_none_since
                        .as_deref()
                        .and_then(|value| value.parse::<u64>().ok());
                    let since = match since {
                        Some(since) => since,
                        None => {
                            current.ci_none_since = Some(now.to_string());
                            now
                        }
                    };
                    Ok(now.saturating_sub(since) >= 600)
                } else {
                    current.ci_none_since = None;
                    Ok(false)
                }
            })?
        } else {
            false
        };
        if ticket.state == TicketState::CiWait && (checks == Checks::Passing || no_checks_ready) {
            let _hooks = crate::hooks::defer();
            let _dispatch = crate::ticket::revise::dispatch_lock(store, &ticket.id)?;
            // Direct and CLI launches use admission rather than dispatch.
            // Drain them before retiring the flock and making it inactive.
            let _operation = crate::ticket::operation::exclusive(store, &ticket.id)?;
            if !store
                .read()?
                .tickets
                .iter()
                .any(|current| same_ci_generation(current, ticket))
            {
                return Ok(false);
            }
            crate::ticket::merge::retire_flock(store, h, &ticket.id)?;
            let changed = store.update(|data| {
                let Some(current) = data
                    .tickets
                    .iter_mut()
                    .find(|current| same_ci_generation(current, ticket))
                else {
                    return Ok(false);
                };
                crate::ticket::state::transition(
                    current,
                    TicketState::PrOpen,
                    None,
                    &now.to_string(),
                )?;
                current.ci_none_since = None;
                Ok(true)
            })?;
            if changed {
                crate::notify::Notifier::new(h, cfg.upkeep.notifications).send(
                    &format!("{}: PR #{} ready for review", ticket.id, status.number),
                    &status.url,
                );
                crate::hooks::emit(
                    cfg,
                    "ticket.stage_changed",
                    &serde_json::json!({"ticket":ticket.id,"from":"ci_wait","to":"pr_open"}),
                );
            }
            return Ok(changed);
        }
        Ok(false)
    })();
    let pickup = crate::ticket::pr::comments(store, gh, &ticket.id, false)
        .and_then(|_| notify_comments(h, store, cfg, &ticket.id));
    if let Err(error) = pickup {
        eprintln!("shepherd: {}: PR comment pickup: {error}", ticket.id);
    }
    ci_result
}

fn same_ci_generation(current: &Ticket, observed: &Ticket) -> bool {
    current.id == observed.id
        && current.state == TicketState::CiWait
        && current.pr_number == observed.pr_number
        && current.ci_head_sha == observed.ci_head_sha
        && current.history.len() == observed.history.len()
        && current.updated_at == observed.updated_at
}

fn notify_comments(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    id: &str,
) -> Result<(), ShepherdError> {
    if !store
        .read()?
        .tickets
        .iter()
        .any(|ticket| ticket.id == id && ticket.state == TicketState::PrOpen)
    {
        return Ok(());
    }
    let count = store.update(|data| {
        let Some(ticket) = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id && ticket.state == TicketState::PrOpen)
        else {
            return Ok(0);
        };
        let mut count = 0;
        for comment in &mut ticket.comments {
            if !comment.triaged && !comment.pinged {
                comment.pinged = true;
                count += 1;
            }
        }
        if count > 0 {
            data.enqueue_lead_notice(
                &format!("{id}: {count} new PR comment(s), run ticket comments {id} --new"),
                Some(id),
                None,
            );
        }
        Ok(count)
    })?;
    if count > 0 {
        crate::hooks::emit(
            cfg,
            "ticket.pr_comment",
            &serde_json::json!({"ticket":id,"count":count}),
        );
    }
    let data = store.read()?;
    let pending = data
        .pending_pings
        .iter()
        .any(|text| text.contains(&format!("run ticket comments {id} --new")));
    if pending {
        let live = data
            .lead
            .as_ref()
            .map(|lead| h.agent_get(&lead.agent_name))
            .transpose()?
            .flatten();
        if live.is_none() {
            let workspaces = h.workspaces()?;
            let workspace = data
                .lead
                .as_ref()
                .and_then(|lead| lead.workspace_id.as_deref())
                .filter(|id| {
                    workspaces
                        .iter()
                        .any(|workspace| workspace.workspace_id == *id)
                })
                .or_else(|| {
                    data.tickets
                        .iter()
                        .find(|ticket| ticket.id == id)
                        .and_then(|ticket| ticket.workspace_id.as_deref())
                        .filter(|id| {
                            workspaces
                                .iter()
                                .any(|workspace| workspace.workspace_id == *id)
                        })
                })
                .ok_or_else(|| {
                    ShepherdError::Usage("no recorded workspace available to resume lead".into())
                })?;
            let state = store
                .root()
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| ShepherdError::Usage("invalid project store path".into()))?;
            let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
            crate::lead::resume(
                h,
                store,
                cfg,
                &crate::lead::LeadContext {
                    bin: &bin,
                    state_dir: state,
                    workspace_id: workspace,
                },
            )?;
        }
    }
    crate::lead::flush_pings(h, store)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        github::{Checks, PrState, PrStatus, ReviewDecision, fake::FakeBackend},
        herdr::{AgentInfo, Workspace, WorktreeInfo, fake::FakeHerdr},
        store::{LeadRecord, Store, Ticket, TicketState},
    };
    use std::{path::PathBuf, process::Command};

    struct Fixture {
        _dir: tempfile::TempDir,
        state: PathBuf,
        store: Store,
        cfg: Config,
        h: FakeHerdr,
    }
    impl Fixture {
        fn new(state: TicketState) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let output = Command::new("git")
                .args(["init", "--quiet", "--initial-branch=main"])
                .arg(&root)
                .output()
                .unwrap();
            assert!(output.status.success());
            let state_dir = dir.path().join("state");
            crate::trusted::ensure_snapshot(&state_dir, &root).unwrap();
            let store = Store::open(&state_dir, &root).unwrap();
            store
                .update(|data| {
                    data.tickets.push(Ticket {
                        id: "T1".into(),
                        branch: "feat/t1".into(),
                        base: "main".into(),
                        state,
                        pr_number: Some(1),
                        ci_head_sha: Some("fixture-head".into()),
                        worktree: Some(root.clone()),
                        workspace_id: Some("w1".into()),
                        ..Ticket::default()
                    });
                    data.lead = Some(LeadRecord {
                        agent_name: "lead".into(),
                        tab_id: Some("w1:t1".into()),
                        pane_id: Some("w1:p1".into()),
                        started_at: "0".into(),
                        workspace_id: Some("w1".into()),
                    });
                    Ok(())
                })
                .unwrap();
            let h = FakeHerdr::new(vec![Workspace {
                workspace_id: "w1".into(),
                label: "ticket".into(),
                worktree: Some(WorktreeInfo {
                    repo_key: "fixture".into(),
                    repo_root: Some(root.clone()),
                    checkout_path: Some(root.clone()),
                    is_linked_worktree: true,
                }),
            }]);
            h.agent_records.borrow_mut().push(AgentInfo {
                name: Some("lead".into()),
                agent: "codex".into(),
                agent_status: "idle".into(),
                cwd: Some(root),
                pane_id: "w1:p1".into(),
                workspace_id: Some("w1".into()),
            });
            let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
            Self {
                _dir: dir,
                state: state_dir,
                store,
                cfg,
                h,
            }
        }
        fn status(checks: Checks) -> PrStatus {
            PrStatus {
                number: 1,
                url: "https://example.invalid/pull/1".into(),
                state: PrState::Open,
                merged_at: None,
                base: "main".into(),
                head: "feat/t1".into(),
                head_oid: Some("fixture-head".into()),
                checks,
                review: ReviewDecision::None,
                comment_ids: vec![],
            }
        }
        fn tick(&self, gh: &FakeBackend, now: u64) -> super::super::TickReport {
            tick_tickets(&self.h, gh, &self.cfg, &self.state, now)
        }
        fn ticket(&self) -> Ticket {
            self.store.read().unwrap().tickets[0].clone()
        }
        fn reported_agent(&self) {
            self.store
                .update(|data| {
                    data.agents.push(crate::store::AgentRecord {
                        name: "qa".into(),
                        ticket: "T1".into(),
                        role: "qa".into(),
                        runner: "codex-sol-high".into(),
                        status: crate::store::AgentStatus::Reported,
                        pane_id: Some("w1:p2".into()),
                        tab_id: Some("w1:t2".into()),
                        worktree: data.project_root.clone(),
                        task: "Verify".into(),
                        report: None,
                        observed: None,
                        last_progress_at: "0".into(),
                        nudges: 0,
                        restarts: 0,
                        prompt_pending: false,
                    });
                    Ok(())
                })
                .unwrap();
            self.h
                .pane_cwds
                .borrow_mut()
                .insert("w1:p2".into(), self.store.read().unwrap().project_root);
        }
    }

    struct CommentFailureBackend {
        fake: FakeBackend,
        store: Store,
        fail_comments: std::cell::Cell<bool>,
        pickup_states: std::cell::RefCell<Vec<TicketState>>,
        checks_observed: Option<std::sync::mpsc::Sender<()>>,
    }
    impl PrBackend for CommentFailureBackend {
        fn failed_checks(
            &self,
            _path: &Path,
            _number: u64,
        ) -> Result<Vec<crate::github::FailedCheck>, ShepherdError> {
            if let Some(sender) = &self.checks_observed {
                sender.send(()).unwrap();
            }
            Ok(vec![])
        }
        fn view(&self, path: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError> {
            self.fake.view(path, branch)
        }
        fn comments(
            &self,
            path: &Path,
            number: u64,
        ) -> Result<Vec<crate::github::PrComment>, ShepherdError> {
            self.pickup_states
                .borrow_mut()
                .push(self.store.read()?.tickets[0].state);
            if self.fail_comments.replace(false) {
                return Err(ShepherdError::GithubUnavailable(
                    "comment fixture unavailable".into(),
                ));
            }
            self.fake.comments(path, number)
        }
        fn push(&self, gate: &crate::gate::Gate) -> Result<(), ShepherdError> {
            self.fake.push(gate)
        }
        fn create(
            &self,
            gate: &crate::gate::Gate,
            base: &str,
            title: &str,
            body: &str,
        ) -> Result<PrStatus, ShepherdError> {
            self.fake.create(gate, base, title, body)
        }
        fn add_reviewers(
            &self,
            gate: &crate::gate::Gate,
            number: u64,
            reviewers: &[String],
        ) -> Result<(), ShepherdError> {
            self.fake.add_reviewers(gate, number, reviewers)
        }
        fn reply(
            &self,
            gate: &crate::gate::Gate,
            number: u64,
            comment: Option<u64>,
            body: &str,
        ) -> Result<(), ShepherdError> {
            self.fake.reply(gate, number, comment, body)
        }
    }

    #[test]
    fn ci_failure_waits_for_dispatch_and_rechecks_generation() {
        use std::{sync::mpsc, time::Duration};
        let fixture = Fixture::new(TicketState::CiWait);
        let store = fixture.store.clone();
        let dispatch = crate::ticket::revise::dispatch_lock(&store, "T1").unwrap();
        let (checks_tx, checks_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let gh = CommentFailureBackend {
                fake: FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Failing)))]),
                store: fixture.store.clone(),
                fail_comments: std::cell::Cell::new(false),
                pickup_states: Default::default(),
                checks_observed: Some(checks_tx),
            };
            let result = tick_tickets(&fixture.h, &gh, &fixture.cfg, &fixture.state, 100);
            finished_tx.send(()).unwrap();
            (fixture, result)
        });
        checks_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let waiting = matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let before_release = store.read().unwrap().tickets[0].state;
        if before_release == TicketState::CiWait {
            store
                .update(|data| {
                    let ticket = &mut data.tickets[0];
                    crate::ticket::state::transition(ticket, TicketState::PrOpen, None, "101")?;
                    crate::ticket::state::transition(ticket, TicketState::Revising, None, "102")?;
                    Ok(())
                })
                .unwrap();
        }
        drop(dispatch);
        let (fixture, result) = worker.join().unwrap();
        assert!(waiting);
        assert_eq!(
            before_release,
            TicketState::CiWait,
            "red CI changed state without the dispatch lock"
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.updated, 0);
        assert_eq!(fixture.ticket().state, TicketState::Revising);
        assert_eq!(fixture.ticket().revision_round, 0);
    }

    #[test]
    fn comment_outage_does_not_block_ci_and_retries_pickup() {
        for (checks, expected, round) in [
            (Checks::Passing, TicketState::PrOpen, 0),
            (Checks::Failing, TicketState::Building, 1),
        ] {
            let fixture = Fixture::new(TicketState::CiWait);
            let fake = FakeBackend::new(vec![
                Ok(Some(Fixture::status(checks))),
                Ok(Some(Fixture::status(checks))),
            ]);
            *fake.comments.borrow_mut() = crate::github::gh::parse_comments(
                include_str!("../../tests/fixtures/gh_comments_nullable.json"),
                true,
            )
            .unwrap();
            let gh = CommentFailureBackend {
                fake,
                store: fixture.store.clone(),
                fail_comments: std::cell::Cell::new(true),
                pickup_states: Default::default(),
                checks_observed: None,
            };
            let report = tick_tickets(&fixture.h, &gh, &fixture.cfg, &fixture.state, 100);
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(report.updated, 1);
            assert_eq!(fixture.ticket().state, expected);
            assert_eq!(fixture.ticket().revision_round, round);
            assert_eq!(*gh.pickup_states.borrow(), [expected]);
            assert!(fixture.ticket().comments.is_empty());
            let report = tick_tickets(&fixture.h, &gh, &fixture.cfg, &fixture.state, 101);
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            let ticket = fixture.ticket();
            assert_eq!(ticket.comments.len(), 4);
            assert_eq!(ticket.comments[0].author, "ghost");
            assert_eq!(ticket.comments[1].body, "");
        }
    }

    #[test]
    fn stale_red_head_does_not_bounce_or_consume_round() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture
            .store
            .update(|data| {
                data.tickets[0].ci_head_sha = Some("new-head".into());
                data.tickets[0].ci_none_since = Some("1".into());
                Ok(())
            })
            .unwrap();
        let mut status = Fixture::status(Checks::Failing);
        status.head_oid = Some("old-head".into());
        let gh = FakeBackend::new(vec![Ok(Some(status))]);
        let report = fixture.tick(&gh, 1000);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let ticket = fixture.ticket();
        assert_eq!(ticket.state, TicketState::CiWait);
        assert_eq!(ticket.revision_round, 0);
        assert!(ticket.ci_failure_task.is_none());
        assert!(ticket.ci_none_since.is_none());
        assert!(
            !gh.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("failed_checks"))
        );
    }

    #[test]
    fn stale_green_head_does_not_hand_off() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture
            .store
            .update(|data| {
                data.tickets[0].ci_head_sha = Some("new-head".into());
                Ok(())
            })
            .unwrap();
        fixture.reported_agent();
        let mut status = Fixture::status(Checks::Passing);
        status.head_oid = Some("old-head".into());
        let gh = FakeBackend::new(vec![Ok(Some(status))]);
        let report = fixture.tick(&gh, 1000);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fixture.ticket().state, TicketState::CiWait);
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            crate::store::AgentStatus::Reported
        );
    }

    #[test]
    fn unknown_pushed_head_keeps_ci_pending() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture
            .store
            .update(|data| {
                data.tickets[0].ci_head_sha = None;
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        let report = fixture.tick(&gh, 1000);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fixture.ticket().state, TicketState::CiWait);
    }

    #[test]
    fn newer_ci_generation_ignores_old_pass() {
        newer_ci_generation_ignores_observation(Checks::Passing);
    }

    #[test]
    fn newer_ci_generation_ignores_old_failure() {
        newer_ci_generation_ignores_observation(Checks::Failing);
    }

    fn newer_ci_generation_ignores_observation(checks: Checks) {
        let fixture = Fixture::new(TicketState::CiWait);
        let stale = fixture.ticket();
        fixture.reported_agent();
        fixture
            .store
            .update(|data| {
                let ticket = &mut data.tickets[0];
                crate::ticket::state::transition(ticket, TicketState::PrOpen, None, "101")?;
                crate::ticket::state::transition(ticket, TicketState::Revising, None, "102")?;
                crate::ticket::state::transition(ticket, TicketState::CiWait, None, "103")?;
                Ok(())
            })
            .unwrap();
        let current = fixture.ticket();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(checks)))]);
        assert!(!poll_ticket(&fixture.h, &gh, &fixture.store, &fixture.cfg, &stale, 104).unwrap());
        assert_eq!(fixture.ticket(), current);
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            crate::store::AgentStatus::Reported
        );
    }

    #[test]
    fn interrupted_workers_reach_all_tickets_and_sidebars() {
        use std::{
            cell::RefCell,
            panic::{AssertUnwindSafe, catch_unwind},
        };
        struct Interrupted(RefCell<Vec<String>>, std::cell::Cell<bool>);
        impl PrBackend for Interrupted {
            fn view(&self, _: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError> {
                self.0.borrow_mut().push(branch.into());
                // Model a hard deadline without waiting for the real 180 seconds.
                if self.1.get() || branch == "feat/t1" {
                    panic!("simulated worker termination");
                }
                let mut status = Fixture::status(Checks::Passing);
                status.head = branch.into();
                if let Some(number) = branch.strip_prefix("feat/t") {
                    status.number = number.parse().unwrap();
                }
                Ok(Some(status))
            }
            fn comments(
                &self,
                _: &Path,
                _: u64,
            ) -> Result<Vec<crate::github::PrComment>, ShepherdError> {
                Ok(vec![])
            }
            fn push(&self, _: &crate::gate::Gate) -> Result<(), ShepherdError> {
                unreachable!()
            }
            fn create(
                &self,
                _: &crate::gate::Gate,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<PrStatus, ShepherdError> {
                unreachable!()
            }
            fn add_reviewers(
                &self,
                _: &crate::gate::Gate,
                _: u64,
                _: &[String],
            ) -> Result<(), ShepherdError> {
                unreachable!()
            }
            fn reply(
                &self,
                _: &crate::gate::Gate,
                _: u64,
                _: Option<u64>,
                _: &str,
            ) -> Result<(), ShepherdError> {
                unreachable!()
            }
        }
        let fixture = Fixture::new(TicketState::CiWait);
        fixture
            .store
            .update(|data| {
                for number in 2..=4 {
                    let mut ticket = data.tickets[0].clone();
                    ticket.id = format!("T{number}");
                    ticket.branch = format!("feat/t{number}");
                    ticket.pr_number = Some(number);
                    data.tickets.push(ticket);
                }
                Ok(())
            })
            .unwrap();
        let other = Fixture::new(TicketState::CiWait);
        let root = other.store.read().unwrap().project_root;
        crate::trusted::ensure_snapshot(&fixture.state, &root).unwrap();
        let other_store = Store::open(&fixture.state, &root).unwrap();
        other_store
            .update(|data| {
                let mut ticket = other.ticket();
                ticket.id = "T5".into();
                ticket.branch = "feat/t5".into();
                ticket.pr_number = Some(5);
                data.tickets.push(ticket);
                Ok(())
            })
            .unwrap();
        for number in 2..=3 {
            let root = fixture._dir.path().join(format!("sidebar-{number}"));
            std::fs::create_dir(&root).unwrap();
            assert!(
                Command::new("git")
                    .args(["init", "--quiet", "--initial-branch"])
                    .arg(format!("side/{number}"))
                    .arg(&root)
                    .status()
                    .unwrap()
                    .success()
            );
            fixture.h.workspaces.borrow_mut().push(Workspace {
                workspace_id: format!("w{number}"),
                label: "sidebar".into(),
                worktree: Some(WorktreeInfo {
                    repo_key: "fixture".into(),
                    repo_root: None,
                    checkout_path: Some(root),
                    is_linked_worktree: true,
                }),
            });
        }
        let backend = Interrupted(RefCell::new(vec![]), std::cell::Cell::new(true));
        let cycle = || {
            let mut state = super::super::WatchState::load(&fixture.state).unwrap();
            state.state_dir = Some(fixture.state.clone());
            assert!(
                catch_unwind(AssertUnwindSafe(|| super::super::tick(
                    &fixture.h,
                    &backend,
                    &fixture.cfg,
                    &crate::notify::Notifier::new(&fixture.h, false),
                    &mut state,
                )))
                .is_err()
            );
            // Discard memory exactly as the resident watcher's killed worker does.
        };
        for _ in 0..10 {
            cycle();
        }
        let calls = backend.0.borrow();
        assert_eq!(calls.len(), 10);
        for branch in [
            "feat/t1", "feat/t2", "feat/t3", "feat/t4", "feat/t5", "main", "side/2", "side/3",
        ] {
            assert!(
                calls.iter().any(|call| call == branch),
                "starved {branch}: {calls:?}"
            );
        }
        assert_eq!(
            calls
                .iter()
                .filter(|branch| branch.starts_with("feat/"))
                .count(),
            5
        );
        // Removing the cursor's ticket must not restart the surviving prefix.
        let removed = calls
            .iter()
            .rev()
            .find(|branch| branch.starts_with("feat/"))
            .unwrap()
            .clone();
        drop(calls);
        for store in [&fixture.store, &other_store] {
            store
                .update(|data| {
                    data.tickets.retain(|ticket| ticket.branch != removed);
                    Ok(())
                })
                .unwrap();
        }
        backend.0.borrow_mut().clear();
        for _ in 0..8 {
            cycle();
        }
        let calls = backend.0.borrow();
        assert_eq!(calls.len(), 8);
        for branch in [
            "feat/t1", "feat/t2", "feat/t3", "feat/t4", "feat/t5", "main", "side/2", "side/3",
        ] {
            assert_eq!(
                calls.iter().any(|call| call == branch),
                branch != removed,
                "{calls:?}"
            );
        }
        drop(calls);
        // A persistently slow first ticket must not prevent healthy tickets
        // from completing their CI handoff or sidebar tokens from being saved.
        backend.1.set(false);
        for _ in 0..4 {
            cycle();
        }
        for store in [&fixture.store, &other_store] {
            for ticket in store.read().unwrap().tickets {
                assert_eq!(
                    ticket.state,
                    if ticket.branch == "feat/t1" {
                        TicketState::CiWait
                    } else {
                        TicketState::PrOpen
                    },
                    "{}",
                    ticket.id
                );
            }
        }
        assert_eq!(
            super::super::WatchState::load(&fixture.state)
                .unwrap()
                .owned_tokens
                .len(),
            3
        );
        assert_eq!(
            fixture
                .h
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("workspace_tokens "))
                .count(),
            3
        );
    }

    #[test]
    fn sidebar_disabled_still_polls_ticket_lifecycle() {
        let mut fixture = Fixture::new(TicketState::CiWait);
        fixture.cfg.upkeep.pr_status = false;
        fixture.h.workspaces.borrow_mut().clear();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        let mut state = super::super::WatchState {
            state_dir: Some(fixture.state.clone()),
            ..Default::default()
        };
        let report = super::super::tick(
            &fixture.h,
            &gh,
            &fixture.cfg,
            &crate::notify::Notifier::new(&fixture.h, true),
            &mut state,
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fixture.ticket().state, TicketState::PrOpen);
    }

    #[test]
    fn stale_ci_pass_does_not_retire_revision_flock() {
        let fixture = Fixture::new(TicketState::CiWait);
        let stale = fixture.ticket();
        fixture.reported_agent();
        fixture
            .store
            .update(|data| {
                data.tickets[0].state = TicketState::Revising;
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        assert!(!poll_ticket(&fixture.h, &gh, &fixture.store, &fixture.cfg, &stale, 100).unwrap());
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            crate::store::AgentStatus::Reported
        );
        assert_eq!(fixture.ticket().state, TicketState::Revising);
    }

    #[test]
    fn ci_handoff_retires_ticket_flock() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture.reported_agent();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        assert!(fixture.tick(&gh, 100).errors.is_empty());
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            crate::store::AgentStatus::Retired
        );
        assert!(
            fixture
                .h
                .calls
                .borrow()
                .iter()
                .any(|call| call == "close_tab w1:t2")
        );
        assert!(fixture.store.read().unwrap().project_root.exists());
    }

    #[test]
    fn ci_handoff_drains_nonrevision_admission() {
        for checks in [Checks::Passing, Checks::None] {
            let fixture = Fixture::new(TicketState::CiWait);
            fixture.reported_agent();
            let store = fixture.store.clone();
            let agent = store
                .update(|data| {
                    data.tickets[0].ci_none_since = Some("0".into());
                    Ok(data.agents.remove(0))
                })
                .unwrap();
            // A direct or CLI launch owns shared admission before its agent
            // record is persisted. Complete it while handoff is waiting.
            let admission = crate::ticket::operation::shared(&store, "T1").unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (finished_tx, finished_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(checks)))]);
                started_tx.send(()).unwrap();
                let report = fixture.tick(&gh, 1000);
                finished_tx.send(()).unwrap();
                (fixture, report)
            });
            started_rx.recv().unwrap();
            let early = finished_rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_ok();
            store
                .update(|data| {
                    data.agents.push(agent);
                    Ok(())
                })
                .unwrap();
            drop(admission);
            let (fixture, report) = worker.join().unwrap();
            assert!(
                !early,
                "CI handoff skipped an admitted nonrevision launch: {checks:?}"
            );
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(fixture.ticket().state, TicketState::PrOpen);
            assert_eq!(
                store.read().unwrap().agents[0].status,
                crate::store::AgentStatus::Retired,
                "handoff must retire the admitted launch before becoming inactive"
            );
            assert!(
                fixture
                    .h
                    .calls
                    .borrow()
                    .iter()
                    .any(|call| call == "close_tab w1:t2")
            );
        }
    }

    #[test]
    fn automatic_ci_handoff_defers_unknown_or_busy_flock() {
        for blocked_by in ["unknown", "busy"] {
            let fixture = Fixture::new(TicketState::CiWait);
            fixture.reported_agent();
            fixture
                .store
                .update(|data| {
                    data.agents[0].status = crate::store::AgentStatus::Blocked;
                    if blocked_by == "unknown" {
                        data.agents[0].set_prompt_delivery_uncertain(true);
                    }
                    Ok(())
                })
                .unwrap();
            let _operation = (blocked_by == "busy").then(|| {
                crate::flock::try_agent_operation(&fixture.store, "qa")
                    .unwrap()
                    .unwrap()
            });
            for now in [100, 101] {
                let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
                assert!(!fixture.tick(&gh, now).errors.is_empty());
                assert_eq!(fixture.ticket().state, TicketState::CiWait);
                assert_ne!(
                    fixture.store.read().unwrap().agents[0].status,
                    crate::store::AgentStatus::Retired
                );
            }
            assert!(
                !fixture
                    .h
                    .calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("send_keys ")
                        || call.starts_with("close_tab ")
                        || call.starts_with("remove_worktree "))
            );
        }
    }

    #[test]
    fn ci_loop_cap_notifies() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture
            .store
            .update(|data| {
                data.tickets[0].revision_round = 3;
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Failing)))]);
        assert!(fixture.tick(&gh, 100).errors.is_empty());
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
        assert!(
            fixture
                .h
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("CI revision loop cap"))
        );
    }

    #[test]
    fn watch_tick_polls_stored_tickets() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture.h.workspaces.borrow_mut().clear();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        let mut state = super::super::WatchState {
            state_dir: Some(fixture.state.clone()),
            ..Default::default()
        };
        let report = super::super::tick(
            &fixture.h,
            &gh,
            &fixture.cfg,
            &crate::notify::Notifier::new(&fixture.h, true),
            &mut state,
        );
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fixture.ticket().state, TicketState::PrOpen);
    }

    #[test]
    fn comment_during_building_is_deferred() {
        let fixture = Fixture::new(TicketState::Building);
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        gh.comments.borrow_mut().push(crate::github::PrComment {
            kind: None,
            id: 9,
            author: "reviewer".into(),
            path: None,
            line: None,
            body: "Check the revision".into(),
            review: false,
        });
        assert!(fixture.tick(&gh, 100).errors.is_empty());
        assert_eq!(fixture.ticket().comments.len(), 1);
        assert!(!fixture.ticket().comments[0].pinged);
        assert!(
            !fixture
                .h
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("run ticket comments"))
        );
    }

    #[test]
    fn watcher_merged_during_revision_keeps_worktree() {
        let fixture = Fixture::new(TicketState::Revising);
        let root = fixture.store.read().unwrap().project_root;
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["symbolic-ref", "HEAD", "refs/heads/feat/t1"])
                .status()
                .unwrap()
                .success()
        );
        let mut status = Fixture::status(Checks::Passing);
        status.state = PrState::Merged;
        status.merged_at = Some("2026-11-01T00:00:00Z".into());
        let gh = FakeBackend::new(vec![Ok(Some(status))]);
        let report = fixture.tick(&gh, 100);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
        assert!(root.exists());
        assert!(
            !gh.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("push "))
        );
    }

    #[test]
    fn lead_resumed_when_missing() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fixture.h.agent_records.borrow_mut().clear();
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        gh.comments.borrow_mut().push(crate::github::PrComment {
            kind: None,
            id: 7,
            author: "reviewer".into(),
            path: None,
            line: None,
            body: "Check this behavior".into(),
            review: false,
        });
        let report = fixture.tick(&gh, 100);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            fixture
                .h
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("start_agent "))
        );
        assert_ne!(
            fixture.store.read().unwrap().lead.unwrap().agent_name,
            "lead"
        );
    }

    #[test]
    fn comment_during_ci_wait_deferred_then_pinged_once() {
        let fixture = Fixture::new(TicketState::CiWait);
        fixture
            .store
            .update(|data| {
                data.agents.push(crate::store::AgentRecord {
                    name: "lead".into(),
                    ticket: String::new(),
                    role: "lead".into(),
                    runner: "codex-sol-high".into(),
                    status: crate::store::AgentStatus::Working,
                    pane_id: Some("w1:p1".into()),
                    tab_id: Some("w1:t1".into()),
                    worktree: data.project_root.clone(),
                    task: "coordinate".into(),
                    report: None,
                    observed: None,
                    last_progress_at: "0".into(),
                    nudges: 0,
                    restarts: 0,
                    prompt_pending: false,
                });
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![
            Ok(Some(Fixture::status(Checks::Pending))),
            Ok(Some(Fixture::status(Checks::Passing))),
            Ok(Some(Fixture::status(Checks::Passing))),
        ]);
        gh.comments.borrow_mut().push(crate::github::PrComment {
            kind: None,
            id: 7,
            author: "reviewer".into(),
            path: Some("src/main.rs".into()),
            line: Some(1),
            body: "Fix the check".into(),
            review: true,
        });
        assert!(fixture.tick(&gh, 100).errors.is_empty());
        assert_eq!(fixture.ticket().comments.len(), 1);
        assert!(
            !fixture
                .h
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("run ticket comments"))
        );
        assert!(fixture.tick(&gh, 101).errors.is_empty());
        assert!(fixture.tick(&gh, 102).errors.is_empty());
        assert_eq!(fixture.ticket().comments.len(), 1);
        assert_eq!(
            fixture
                .h
                .calls
                .borrow()
                .iter()
                .filter(|call| call.contains("run ticket comments"))
                .count(),
            1
        );
        assert!(!fixture.ticket().comments[0].triaged);
    }

    #[test]
    fn gh_outage_no_change() {
        let fixture = Fixture::new(TicketState::CiWait);
        let before = fixture.ticket();
        let gh = FakeBackend::new(vec![Err(ShepherdError::GithubUnavailable(
            "offline".into(),
        ))]);
        assert_eq!(fixture.tick(&gh, 100).errors.len(), 1);
        assert_eq!(fixture.ticket(), before);
        assert!(fixture.h.calls.borrow().is_empty());
    }

    #[test]
    fn ci_none_timeout() {
        let fixture = Fixture::new(TicketState::CiWait);
        let gh = FakeBackend::new(vec![
            Ok(Some(Fixture::status(Checks::None))),
            Ok(Some(Fixture::status(Checks::None))),
            Ok(Some(Fixture::status(Checks::Pending))),
            Ok(Some(Fixture::status(Checks::None))),
            Ok(Some(Fixture::status(Checks::None))),
            Ok(Some(Fixture::status(Checks::None))),
        ]);
        for now in [100, 699, 700, 800, 1399] {
            assert!(fixture.tick(&gh, now).errors.is_empty());
            assert_eq!(fixture.ticket().state, TicketState::CiWait);
        }
        assert!(fixture.tick(&gh, 1400).errors.is_empty());
        assert_eq!(fixture.ticket().state, TicketState::PrOpen);
    }

    #[test]
    fn ci_fail_bounces_to_building() {
        let fixture = Fixture::new(TicketState::CiWait);
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Failing)))]);
        assert!(fixture.tick(&gh, 100).errors.is_empty());
        let ticket = fixture.ticket();
        assert_eq!(ticket.state, TicketState::Building);
        assert_eq!(ticket.revision_round, 1);
        assert!(
            ticket
                .ci_failure_task
                .unwrap()
                .contains("https://example.invalid/pull/1")
        );
    }

    #[test]
    fn ci_pass_hands_off() {
        let fixture = Fixture::new(TicketState::CiWait);
        let gh = FakeBackend::new(vec![Ok(Some(Fixture::status(Checks::Passing)))]);
        assert!(fixture.tick(&gh, 100).errors.is_empty());
        assert_eq!(fixture.ticket().state, TicketState::PrOpen);
        assert!(
            fixture
                .h
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("PR #1 ready for review"))
        );
    }
}
