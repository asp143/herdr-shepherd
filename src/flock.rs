use crate::{error::ShepherdError, store::Report};

pub fn validate_report(text: &str) -> Result<Report, ShepherdError> {
    let report: Report = serde_json::from_str(text)
        .map_err(|e| ShepherdError::Usage(format!("invalid report: {e}")))?;
    if !matches!(report.status.as_str(), "completed" | "blocked" | "failed") {
        return Err(ShepherdError::Usage(
            "report status must be completed, blocked, or failed".into(),
        ));
    }
    Ok(report)
}

/// Validate and persist a report while holding the project lock.
pub fn store_report(
    store: &crate::store::Store,
    name: &str,
    text: &str,
) -> Result<crate::store::AgentRecord, ShepherdError> {
    use crate::store::AgentStatus;
    let report = validate_report(text)?;
    store.update(|data| {
        let agent = data
            .agents
            .iter_mut()
            .find(|a| a.name == name)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown agent: {name}")))?;
        if !matches!(
            agent.status,
            AgentStatus::Starting
                | AgentStatus::Working
                | AgentStatus::Blocked
                | AgentStatus::Reported
        ) {
            return Err(ShepherdError::Usage("agent has been retired".into()));
        }
        if let Some(old) = &agent.report {
            if old == &report {
                return Ok(agent.clone());
            }
            return Err(ShepherdError::Usage(
                "agent already submitted a different report".into(),
            ));
        }
        agent.status = AgentStatus::Reported;
        agent.prompt_pending = false;
        agent.report = Some(report.clone());
        let rec = agent.clone();
        if let Some(ticket) = data.tickets.iter_mut().find(|t| t.id == rec.ticket) {
            ticket.findings.extend(report.findings.clone());
            if rec.role == "planner" {
                ticket.plan = Some(report.summary.clone());
            }
            if rec.role == "researcher" {
                ticket.research.push(report.summary.clone());
            }
        }
        Ok(rec)
    })
}

#[derive(Default, Debug, serde::Serialize)]
pub struct ReconcileReport {
    pub adopted: usize,
    pub failed: usize,
    pub vanished: usize,
}

/// Recover durable launch intents by their reserved herdr identity.
pub fn reconcile(
    h: &dyn crate::herdr::Herdr,
    store: &crate::store::Store,
) -> Result<ReconcileReport, ShepherdError> {
    use crate::store::AgentStatus;
    let mut result = ReconcileReport::default();
    for rec in store.read()?.agents.into_iter().filter(|r| {
        matches!(
            r.status,
            AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
        )
    }) {
        let live = h.agent_get(&rec.name)?;
        store.update(|data| {
            let Some(current) = data
                .agents
                .iter_mut()
                .find(|r| r.name == rec.name && r.status == rec.status)
            else {
                return Ok(());
            };
            match &live {
                Some(info) => {
                    current.pane_id = Some(info.pane_id.clone());
                    current.status = if info.agent_status == "blocked" {
                        AgentStatus::Blocked
                    } else {
                        AgentStatus::Working
                    };
                    result.adopted += 1;
                }
                None if current.status == AgentStatus::Starting => {
                    current.status = AgentStatus::LaunchFailed;
                    current.observed = Some("launch_failed: role re-queued".into());
                    let message = format!(
                        "{}: {} launch_failed; role re-queued for agent spawn",
                        current.ticket, current.role
                    );
                    data.pending_pings.push(message);
                    result.failed += 1;
                }
                None => {
                    current.status = AgentStatus::Retired;
                    current.observed = Some("vanished".into());
                    result.vanished += 1;
                }
            }
            Ok(())
        })?;
    }
    Ok(result)
}

/// Queue a short lead notification when interactive delivery is unavailable.
pub fn ping(h: &dyn crate::herdr::Herdr, store: &crate::store::Store, text: &str) {
    let text: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect();
    let delivered = (|| -> Result<bool, ShepherdError> {
        let Some(lead) = store.read()?.lead else {
            return Ok(false);
        };
        let Some(live) = h.agent_get(&lead.agent_name)? else {
            return Ok(false);
        };
        if matches!(
            live.agent_status.as_str(),
            "blocked" | "working" | "unknown"
        ) {
            return Ok(false);
        }
        h.prompt_agent(&lead.agent_name, &text, false)?;
        Ok(true)
    })()
    .unwrap_or(false);
    if !delivered
        && let Err(error) = store.update(|data| {
            data.pending_pings.push(text);
            Ok(())
        })
    {
        eprintln!("shepherd: queue lead ping: {error}");
    }
}

pub fn report(
    h: &dyn crate::herdr::Herdr,
    store: &crate::store::Store,
    cfg: &crate::config::Config,
    name: &str,
    text: &str,
) -> Result<crate::store::AgentRecord, ShepherdError> {
    let rec = store_report(store, name, text)?;
    ping(
        h,
        store,
        &format!(
            "{}: {} {} reported: {}",
            rec.ticket,
            rec.role,
            rec.name,
            rec.report.as_ref().map_or("", |r| r.summary.as_str())
        ),
    );
    crate::hooks::emit(
        cfg,
        "agent.retired",
        &serde_json::json!({"ticket":rec.ticket,"agent":rec.name,"role":rec.role}),
    );
    Ok(rec)
}

pub fn spawn(
    h: &dyn crate::herdr::Herdr,
    store: &crate::store::Store,
    cfg: &crate::config::Config,
    req: crate::launch::LaunchRequest,
) -> Result<crate::store::AgentRecord, ShepherdError> {
    let data = store.read()?;
    let ticket = data
        .tickets
        .iter()
        .find(|t| t.id == req.ticket)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {}", req.ticket)))?;
    if !crate::ticket::state::is_active(ticket.state) {
        return Err(ShepherdError::Usage("ticket is not active".into()));
    }
    crate::launch::launch_configured(h, store, req, cfg)
}

pub fn cancel(
    h: &dyn crate::herdr::Herdr,
    store: &crate::store::Store,
    name: &str,
    force: bool,
) -> Result<crate::store::AgentRecord, ShepherdError> {
    use crate::store::AgentStatus;
    let data = store.read()?;
    let rec = data
        .agents
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown agent: {name}")))?;
    if let Some(live) = h.agent_get(name)? {
        let inside = live
            .cwd
            .as_ref()
            .and_then(|p| p.canonicalize().ok())
            .zip(rec.worktree.canonicalize().ok())
            .is_some_and(|(cwd, root)| cwd.starts_with(root));
        if !inside || rec.pane_id.as_deref() != Some(live.pane_id.as_str()) {
            return Err(ShepherdError::PolicyDenied(
                "agent is outside its recorded pane or worktree".into(),
            ));
        }
        let result = h.send_keys(name, &["esc".into()]);
        if !force {
            result?;
        }
    }
    if let Some(tab) = &rec.tab_id {
        h.close_tab(tab)?;
    }
    store.update(|data| {
        let rec = data
            .agents
            .iter_mut()
            .find(|a| a.name == name)
            .expect("record retained");
        rec.status = AgentStatus::Retired;
        Ok(rec.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stored_agent(store: &crate::store::Store) -> crate::store::AgentRecord {
        use crate::store::*;
        let rec = AgentRecord {
            name: "sh-fixture-coder-1".into(),
            ticket: "T-1".into(),
            role: "coder".into(),
            runner: "codex-sol-high".into(),
            status: AgentStatus::Working,
            pane_id: Some("p1".into()),
            tab_id: Some("t1".into()),
            worktree: store.read().unwrap().project_root,
            task: "implement".into(),
            report: None,
            observed: None,
            last_progress_at: "0".into(),
            nudges: 0,
            restarts: 0,
            prompt_pending: false,
        };
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    state: TicketState::Building,
                    ..Ticket::default()
                });
                data.agents.push(rec.clone());
                Ok(())
            })
            .unwrap();
        rec
    }
    fn request(
        root: &std::path::Path,
        state: &std::path::Path,
        cfg: &crate::config::Config,
    ) -> crate::launch::LaunchRequest {
        let resolved = crate::roles::resolve(cfg).unwrap();
        crate::launch::LaunchRequest {
            ticket: "T-1".into(),
            role: resolved
                .roles
                .into_iter()
                .find(|r| r.name == "coder")
                .unwrap(),
            runners: vec![resolved.runners["codex-sol-high"].clone()],
            worktree: root.into(),
            workspace_id: "w1".into(),
            tab_label: "coder".into(),
            prompt: "Implement".into(),
            state_dir: state.into(),
        }
    }
    #[test]
    fn concurrent_reports_both_land() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let first = stored_agent(&store);
        let mut second = first.clone();
        second.name = "sh-fixture-qa-1".into();
        second.role = "qa".into();
        store
            .update(|d| {
                d.agents.push(second.clone());
                Ok(())
            })
            .unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for agent in [&first, &second] {
                let store = &store;
                let barrier = &barrier;
                scope.spawn(move || { barrier.wait();store_report(store,&agent.name,r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap(); });
            }
        });
        assert_eq!(
            store
                .read()
                .unwrap()
                .agents
                .iter()
                .filter(|a| a.report.is_some())
                .count(),
            2
        );
    }
    #[test]
    fn spawn_refuses_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = crate::store::Store::open(&state, dir.path()).unwrap();
        stored_agent(&store);
        let mut cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        cfg.raw = "[shepherd]\nmax_parallel_agents=1".parse().unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        let err = spawn(&h, &store, &cfg, request(dir.path(), &state, &cfg)).unwrap_err();
        assert_eq!(err.code(), "capacity");
        assert!(h.calls.borrow().is_empty());
        assert_eq!(store.read().unwrap().agents.len(), 1);
    }
    #[test]
    fn reconcile_marks_missing_starting_failed() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = crate::store::Store::open(&state, dir.path()).unwrap();
        stored_agent(&store);
        store
            .update(|d| {
                d.agents[0].status = crate::store::AgentStatus::Starting;
                Ok(())
            })
            .unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        assert_eq!(reconcile(&h, &store).unwrap().failed, 1);
        let data = store.read().unwrap();
        assert_eq!(
            data.agents[0].status,
            crate::store::AgentStatus::LaunchFailed
        );
        assert!(
            data.agents[0]
                .observed
                .as_ref()
                .unwrap()
                .contains("re-queued")
        );
    }
    #[test]
    fn no_duplicate_after_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = crate::store::Store::open(&state, dir.path()).unwrap();
        let old = stored_agent(&store);
        store
            .update(|d| {
                d.agents[0].status = crate::store::AgentStatus::Starting;
                Ok(())
            })
            .unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        reconcile(&h, &store).unwrap();
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        let new = spawn(&h, &store, &cfg, request(dir.path(), &state, &cfg)).unwrap();
        assert_ne!(old.name, new.name);
        assert!(new.name.ends_with("-2"));
        assert_eq!(h.agent_records.borrow().len(), 1);
        assert_eq!(store.read().unwrap().agents.len(), 2);
    }
    #[test]
    fn reconcile_adopts_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let rec = stored_agent(&store);
        store
            .update(|d| {
                d.agents[0].status = crate::store::AgentStatus::Starting;
                Ok(())
            })
            .unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        h.agent_records.borrow_mut().push(crate::herdr::AgentInfo {
            name: Some(rec.name),
            agent: "codex".into(),
            agent_status: "working".into(),
            cwd: Some(dir.path().into()),
            pane_id: "recovered-pane".into(),
            workspace_id: Some("w1".into()),
        });
        assert_eq!(reconcile(&h, &store).unwrap().adopted, 1);
        let data = store.read().unwrap();
        assert_eq!(data.agents[0].status, crate::store::AgentStatus::Working);
        assert_eq!(data.agents[0].pane_id.as_deref(), Some("recovered-pane"));
    }
    #[test]
    fn report_stores_findings() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let rec = stored_agent(&store);
        let report = r#"{"status":"completed","summary":"done","tests":["test.sh"],"changed_paths":["hello.sh"],"blockers":[],"findings":[{"severity":"mid","text":"add docs","location":null}],"commit_sha":null}"#;
        store_report(&store, &rec.name, report).unwrap();
        let data = store.read().unwrap();
        assert_eq!(data.agents[0].status, crate::store::AgentStatus::Reported);
        assert_eq!(data.tickets[0].findings[0].text, "add docs");
        assert_eq!(data.agents[0].report.as_ref().unwrap().tests, ["test.sh"]);
    }
    #[test]
    fn report_validates_schema() {
        let error = validate_report(r#"{"summary":"done"}"#).unwrap_err();
        assert_eq!(error.code(), "usage");
        let error = validate_report(r#"{"status":"bogus","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap_err();
        assert_eq!(error.code(), "usage");
    }
}
