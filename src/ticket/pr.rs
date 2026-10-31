//! Lead-facing pull request commands.

use std::{path::Path, process::Command, time::Duration};

use crate::{
    config::Config,
    error::ShepherdError,
    gate::{Gate, check_push, parse_push_args},
    github::{PrBackend, PrStatus, codeowners},
    store::{CommentRecord, Store, Ticket, TicketState},
};

pub const LEAD_PR_COMMANDS: &str = r#"
Use ticket pr <id> --summary-file <p> --changes-file <p> --testing-file <p>
when ready_for_pr to push and open a ready-for-review PR.
Use ticket comments <id> --new to inspect comments awaiting triage.
Check each comment against the actual code and reproduce the issue where possible.
If a comment is not relevant, use ticket reply <id> --comment <cid> --body-file <p>
with the reason and evidence. For a legitimate issue, use ticket revise <id>
--comments <cid,cid> --task-file <p> to send the fix through the flock.
After a revision is pushed, use ticket ack <id> --comment <cid> --body-file <p>
to explain the change and include the pushed commit SHA.
never resolve threads. Never request changes, approve, or merge.
Write plainly in the user's voice. Lead with the point. Avoid canned phrasing
and AI attribution in PR text and replies.
When given only an ID, run ticket fetch <id>, then ticket add. If no ticket source
is configured, ask the user to paste the ticket text.
"#;

pub fn validate_body(body: &str) -> Result<(), ShepherdError> {
    if body.trim().is_empty() || body.chars().count() > 60_000 {
        return Err(ShepherdError::Usage(
            "body must contain 1 to 60000 characters".into(),
        ));
    }
    super::pr_body::check_attribution(body)
}

fn ticket<'a>(data: &'a crate::store::StoreData, id: &str) -> Result<&'a Ticket, ShepherdError> {
    data.tickets
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))
}

fn require_pr(ticket: &Ticket) -> Result<u64, ShepherdError> {
    if !matches!(
        ticket.state,
        TicketState::PrOpen | TicketState::CiWait | TicketState::Revising
    ) {
        return Err(ShepherdError::Usage("ticket has no active PR".into()));
    }
    ticket
        .pr_number
        .ok_or_else(|| ShepherdError::Usage("ticket has no PR number".into()))
}

fn authorize(ticket: &Ticket, gate: &Gate) -> Result<(), ShepherdError> {
    let path = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    if gate.ticket_branch != ticket.branch
        || path.canonicalize().map_err(anyhow::Error::from)?
            != gate.worktree.canonicalize().map_err(anyhow::Error::from)?
    {
        return Err(ShepherdError::PolicyDenied(
            "gate does not match stored ticket".into(),
        ));
    }
    check_push(
        &parse_push_args(&["origin", &ticket.branch]),
        &ticket.branch,
        &gate.policy,
    )
}

fn selected(ticket: &Ticket, id: u64) -> Result<usize, ShepherdError> {
    let found: Vec<_> = ticket
        .comments
        .iter()
        .enumerate()
        .filter(|(_, c)| c.id == id)
        .map(|(i, _)| i)
        .collect();
    match found.as_slice() {
        [index] => Ok(*index),
        [] => Err(ShepherdError::Usage(format!("unknown comment: {id}"))),
        _ => Err(ShepherdError::Usage(format!("ambiguous comment id: {id}"))),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn open(
    store: &Store,
    gh: &dyn PrBackend,
    gate: &Gate,
    cfg: &Config,
    id: &str,
    summary: &str,
    changes: &[String],
    testing: &[String],
    now: &str,
) -> Result<Ticket, ShepherdError> {
    let cap = crate::roles::resolve(cfg)?.shepherd.max_revision_rounds;
    let (result, opened) = store.update(|data| {
        let t = ticket(data, id)?;
        let retry = matches!(t.state, TicketState::CiWait | TicketState::PrOpen)
            && t.pr_number.is_some()
            && !t.pr_reviewers_pending.is_empty();
        if t.state != TicketState::ReadyForPr && !retry {
            return Err(ShepherdError::Usage(
                "ticket pr requires ready_for_pr".into(),
            ));
        }
        if summary.trim().is_empty()
            || changes.is_empty()
            || testing.is_empty()
            || changes
                .iter()
                .chain(testing)
                .any(|item| item.trim().is_empty())
        {
            return Err(ShepherdError::Usage(
                "summary, changes and testing must not be empty".into(),
            ));
        }
        let title = super::pr_body::build_title(t);
        if t.title.trim().is_empty() {
            return Err(ShepherdError::Usage("PR title must not be empty".into()));
        }
        super::pr_body::check_attribution(&title)?;
        let body = super::pr_body::build_body(t, summary, changes, testing);
        validate_body(&body)?;
        authorize(t, gate)?;
        if retry {
            let status = gh.view(&gate.worktree, &t.branch)?.ok_or_else(|| {
                ShepherdError::GithubUnavailable("pending PR could not be read".into())
            })?;
            validate_status(t, &status)?;
            if Some(status.number) != t.pr_number {
                return Err(ShepherdError::GithubUnavailable(
                    "pending PR number changed".into(),
                ));
            }
            return Ok((t.clone(), false));
        }
        let reviewers = reviewers(gh, gate, &t.base)?;
        let existing = gh.view(&gate.worktree, &t.branch)?;
        if let Some(status) = &existing {
            validate_status(t, status)?;
        }
        gh.push(gate)?;
        let status = match existing {
            Some(status) => status,
            None => gh.create(gate, &t.base, &title, &body)?,
        };
        validate_status(t, &status)?;
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        t.pr_number = Some(status.number);
        t.pr_reviewers_pending = reviewers;
        super::state::transition_with_limit(t, TicketState::CiWait, None, now, cap)?;
        Ok((t.clone(), true))
    })?;
    if opened {
        crate::hooks::emit(
            cfg,
            "ticket.pr_opened",
            &serde_json::json!({"ticket":id,"pr":result.pr_number}),
        );
    }
    if result.pr_reviewers_pending.is_empty() {
        return Ok(result);
    }
    // Persist PR ownership before requesting reviewers. A failed request is
    // retried on the existing PR without repeating its state transition/hook.
    let number = result.pr_number.expect("persisted PR");
    let requested = gh.requested_reviewers(gate, number)?;
    let remaining: Vec<_> = result
        .pr_reviewers_pending
        .iter()
        .filter(|owner| {
            !requested
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(owner))
        })
        .cloned()
        .collect();
    if !remaining.is_empty() {
        gh.add_reviewers(gate, number, &remaining)?;
    }
    store.update(|data| {
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        if t.pr_number != Some(number) || t.pr_reviewers_pending != result.pr_reviewers_pending {
            return Err(ShepherdError::Usage(
                "pending review request changed".into(),
            ));
        }
        t.pr_reviewers_pending.clear();
        Ok(t.clone())
    })
}

fn validate_status(t: &Ticket, status: &PrStatus) -> Result<(), ShepherdError> {
    if status.number == 0
        || status.head != t.branch
        || status.base != t.base
        || status.state != crate::github::PrState::Open
    {
        return Err(ShepherdError::GithubUnavailable(
            "PR does not match ticket".into(),
        ));
    }
    Ok(())
}

fn safe_codeowners(root: &Path) -> Result<Option<std::path::PathBuf>, ShepherdError> {
    for name in [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"] {
        let mut current = root.to_path_buf();
        let mut missing = false;
        for part in Path::new(name).components() {
            current.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(ShepherdError::PolicyDenied(
                        "CODEOWNERS path must not contain symlinks".into(),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing = true;
                    break;
                }
                Err(error) => return Err(anyhow::Error::from(error).into()),
            }
        }
        if !missing && current.is_file() {
            return Ok(Some(current));
        }
    }
    Ok(None)
}

fn git(path: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let out = crate::proc::capture(
        Command::new("git").current_dir(path).args(args),
        Duration::from_secs(20),
        None,
    )?;
    if !out.status.success() {
        return Err(anyhow::anyhow!("git: {}", out.stderr.trim()).into());
    }
    Ok(out.stdout.trim().into())
}

fn reviewers(gh: &dyn PrBackend, gate: &Gate, base: &str) -> Result<Vec<String>, ShepherdError> {
    git(&gate.worktree, &["check-ref-format", "--branch", base])?;
    let Some(path) = safe_codeowners(&gate.worktree)? else {
        return Ok(vec![]);
    };
    let rules = codeowners::parse(&std::fs::read_to_string(path).map_err(anyhow::Error::from)?);
    let changed = git(
        &gate.worktree,
        &[
            "diff",
            "--name-only",
            "-z",
            &format!("origin/{base}...HEAD"),
            "--",
        ],
    )?;
    let paths = changed
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let author = gh.author(&gate.worktree)?;
    Ok(codeowners::owners_for(&rules, &paths, Some(&author)))
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ListedComment {
    #[serde(flatten)]
    pub comment: CommentRecord,
    pub seen: bool,
}

pub fn comments(
    store: &Store,
    gh: &dyn PrBackend,
    id: &str,
    new_only: bool,
) -> Result<Vec<ListedComment>, ShepherdError> {
    store.update(|data| {
        let t = ticket(data, id)?;
        let number = require_pr(t)?;
        let worktree = t
            .worktree
            .as_deref()
            .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
        let fresh = gh.comments(worktree, number)?;
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        for c in fresh {
            if !t
                .comments
                .iter()
                .any(|stored| stored.id == c.id && stored.review == c.review)
            {
                t.comments.push(CommentRecord {
                    id: c.id,
                    author: c.author,
                    path: c.path,
                    line: c.line,
                    body: c.body,
                    review: c.review,
                    ..CommentRecord::default()
                });
            }
        }
        Ok(t.comments
            .iter()
            .filter(|c| !new_only || !c.triaged)
            .map(|c| ListedComment {
                comment: c.clone(),
                seen: c.triaged,
            })
            .collect())
    })
}

pub fn reply(
    store: &Store,
    gh: &dyn PrBackend,
    gate: &Gate,
    id: &str,
    cid: u64,
    body: &str,
) -> Result<Ticket, ShepherdError> {
    validate_body(body)?;
    store.update(|data| {
        let t = ticket(data, id)?;
        let number = require_pr(t)?;
        if t.state != TicketState::PrOpen {
            return Err(ShepherdError::Usage("triage requires pr_open".into()));
        }
        let index = selected(t, cid)?;
        if t.comments[index].triaged {
            return Err(ShepherdError::Usage("comment already triaged".into()));
        }
        authorize(t, gate)?;
        gh.reply(gate, number, t.comments[index].review.then_some(cid), body)?;
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        t.comments[index].triaged = true;
        t.comments[index].verdict = Some("declined".into());
        Ok(t.clone())
    })
}

pub fn revise(
    store: &Store,
    id: &str,
    cids: &[u64],
    task: &str,
    now: &str,
    cap: u32,
) -> Result<Ticket, ShepherdError> {
    if task.trim().is_empty() || task.chars().count() > 60_000 || cids.is_empty() {
        return Err(ShepherdError::Usage(
            "revision needs comments and a nonempty task of at most 60000 characters".into(),
        ));
    }
    store.update(|data| {
        let t = ticket(data, id)?;
        require_pr(t)?;
        if t.state != TicketState::PrOpen {
            return Err(ShepherdError::Usage("revision requires pr_open".into()));
        }
        let mut indices = Vec::new();
        for cid in cids {
            let index = selected(t, *cid)?;
            if indices.contains(&index) || t.comments[index].triaged {
                return Err(ShepherdError::Usage(
                    "comments must be unique and untriaged".into(),
                ));
            }
            indices.push(index);
        }
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        super::state::transition_with_limit(t, TicketState::Revising, None, now, cap)?;
        for index in indices {
            t.comments[index].triaged = true;
            t.comments[index].verdict = Some("accepted".into());
        }
        t.revision_task = Some(task.into());
        Ok(t.clone())
    })
}

pub fn ack(
    store: &Store,
    gh: &dyn PrBackend,
    gate: &Gate,
    id: &str,
    cid: u64,
    body: &str,
) -> Result<Ticket, ShepherdError> {
    validate_body(body)?;
    store.update(|data| {
        let t = ticket(data, id)?;
        let number = require_pr(t)?;
        if !matches!(t.state, TicketState::CiWait | TicketState::PrOpen) {
            return Err(ShepherdError::Usage(
                "ack requires a pushed revision".into(),
            ));
        }
        let index = selected(t, cid)?;
        let c = &t.comments[index];
        if c.verdict.as_deref() != Some("accepted")
            || c.acknowledged
            || !c
                .ack_sha
                .as_ref()
                .is_some_and(|sha| !sha.is_empty() && body.contains(sha))
        {
            return Err(ShepherdError::Usage(
                "ack must name the pushed commit SHA for an accepted comment".into(),
            ));
        }
        authorize(t, gate)?;
        gh.reply(gate, number, c.review.then_some(cid), body)?;
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        t.comments[index].acknowledged = true;
        Ok(t.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        github::fake::FakeBackend,
        store::{Store, Ticket, TicketState},
    };

    fn fixture(state: TicketState) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    state,
                    pr_number: Some(7),
                    ..Ticket::default()
                });
                Ok(())
            })
            .unwrap();
        (dir, store)
    }

    #[test]
    fn lead_pr_commands_has_triage_rules() {
        for rule in ["ticket reply", "ticket revise", "never resolve"] {
            assert!(LEAD_PR_COMMANDS.contains(rule));
        }
    }

    #[test]
    fn reply_rejects_empty_huge_attribution() {
        let (dir, store) = fixture(TicketState::PrOpen);
        let backend = FakeBackend::new(vec![]);
        let gate = crate::gate::Gate {
            policy: crate::gate::BranchPolicy::new(&[], None, Some("main")).unwrap(),
            ticket_branch: "feat/t-1".into(),
            worktree: dir.path().into(),
            state_dir: dir.path().join("state"),
            config_dir: dir.path().join("cfg"),
        };
        for (body, code) in [
            (" ".into(), "usage"),
            ("x".repeat(60_001), "usage"),
            ("Generated by Codex".into(), "attribution"),
        ] {
            assert_eq!(
                reply(&store, &backend, &gate, "T-1", 1, &body)
                    .unwrap_err()
                    .code(),
                code
            );
        }
        assert!(backend.calls.borrow().is_empty());
    }
    fn comment(id: u64) -> CommentRecord {
        CommentRecord {
            id,
            author: "reviewer".into(),
            body: "Check the boundary".into(),
            review: true,
            ..CommentRecord::default()
        }
    }

    fn gate(dir: &Path) -> Gate {
        Gate {
            policy: crate::gate::BranchPolicy::new(&[], None, Some("main")).unwrap(),
            ticket_branch: "feat/t-1".into(),
            worktree: dir.into(),
            state_dir: dir.join("state"),
            config_dir: dir.join("cfg"),
        }
    }

    #[test]
    fn pr_requires_ready_for_pr() {
        let (dir, store) = fixture(TicketState::Building);
        let gh = FakeBackend::new(vec![]);
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        assert_eq!(
            open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Summary",
                &["Change".into()],
                &["cargo test".into()],
                "1"
            )
            .unwrap_err()
            .code(),
            "usage"
        );
        assert!(gh.calls.borrow().is_empty());
    }

    #[test]
    fn revise_transitions_and_marks_comments() {
        let (_dir, store) = fixture(TicketState::PrOpen);
        store
            .update(|d| {
                d.tickets[0].comments = vec![comment(1), comment(2)];
                Ok(())
            })
            .unwrap();
        let revised = revise(&store, "T-1", &[1, 2], "Fix the boundary", "2", 3).unwrap();
        assert_eq!(revised.state, TicketState::Revising);
        assert_eq!(revised.revision_task.as_deref(), Some("Fix the boundary"));
        assert!(
            revised
                .comments
                .iter()
                .all(|c| c.triaged && c.verdict.as_deref() == Some("accepted"))
        );
        assert_eq!(revised.history.last().unwrap().to, TicketState::Revising);
    }

    #[test]
    fn revise_rejects_wrong_state_unknown_duplicate_and_triaged() {
        let (_dir, store) = fixture(TicketState::CiWait);
        store
            .update(|d| {
                d.tickets[0].comments = vec![comment(1), comment(2)];
                Ok(())
            })
            .unwrap();
        assert!(revise(&store, "T-1", &[1], "Fix", "2", 3).is_err());
        store
            .update(|d| {
                d.tickets[0].state = TicketState::PrOpen;
                d.tickets[0].comments[1].triaged = true;
                Ok(())
            })
            .unwrap();
        for ids in [vec![], vec![9], vec![1, 1], vec![1, 2]] {
            assert!(revise(&store, "T-1", &ids, "Fix", "2", 3).is_err());
            assert!(!store.read().unwrap().tickets[0].comments[0].triaged);
        }
        assert!(revise(&store, "T-1", &[1], " ", "2", 3).is_err());
    }

    #[test]
    fn comments_new_excludes_triaged() {
        let (dir, store) = fixture(TicketState::PrOpen);
        store
            .update(|d| {
                let t = &mut d.tickets[0];
                t.worktree = Some(dir.path().into());
                let mut seen = comment(1);
                seen.triaged = true;
                t.comments = vec![seen];
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![]);
        gh.comments.borrow_mut().extend([
            crate::github::PrComment {
                id: 1,
                author: "reviewer".into(),
                path: None,
                line: None,
                body: "Check".into(),
                review: true,
            },
            crate::github::PrComment {
                id: 2,
                author: "reviewer".into(),
                path: None,
                line: None,
                body: "New".into(),
                review: false,
            },
        ]);
        let new = comments(&store, &gh, "T-1", true).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].comment.id, 2);
        assert!(!new[0].seen);
        let all = comments(&store, &gh, "T-1", false).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].seen);
    }

    #[test]
    fn reply_declines_once_and_ack_requires_pushed_sha() {
        let (dir, store) = fixture(TicketState::PrOpen);
        store
            .update(|d| {
                let t = &mut d.tickets[0];
                t.branch = "feat/t-1".into();
                t.worktree = Some(dir.path().into());
                t.comments = vec![comment(1), comment(2)];
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![]);
        let gate = gate(dir.path());
        let t = reply(
            &store,
            &gh,
            &gate,
            "T-1",
            1,
            "The caller already checks this in validate_input.",
        )
        .unwrap();
        assert_eq!(t.comments[0].verdict.as_deref(), Some("declined"));
        assert!(reply(&store, &gh, &gate, "T-1", 1, "Repeated reply").is_err());
        assert!(ack(&store, &gh, &gate, "T-1", 2, "Fixed abc123").is_err());
        store
            .update(|d| {
                let c = &mut d.tickets[0].comments[1];
                c.triaged = true;
                c.verdict = Some("accepted".into());
                c.ack_sha = Some("abc123".into());
                Ok(())
            })
            .unwrap();
        assert!(ack(&store, &gh, &gate, "T-1", 2, "Fixed it").is_err());
        let t = ack(
            &store,
            &gh,
            &gate,
            "T-1",
            2,
            "Fixed in abc123; cargo test passes.",
        )
        .unwrap();
        assert!(t.comments[1].acknowledged);
        assert!(ack(&store, &gh, &gate, "T-1", 2, "Fixed in abc123").is_err());
        assert_eq!(gh.calls.borrow().len(), 2);
    }

    #[test]
    fn pr_sets_reviewers_from_codeowners() {
        let (dir, store) = fixture(TicketState::ReadyForPr);
        let git_cmd = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
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
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git_cmd(&["init", "-b", "main"]);
        std::fs::write(
            dir.path().join("CODEOWNERS"),
            "*.rs @fixture-author @reviewer @org/team\n",
        )
        .unwrap();
        git_cmd(&["add", "CODEOWNERS"]);
        git_cmd(&["commit", "-m", "base"]);
        git_cmd(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git_cmd(&["switch", "-c", "feat/t-1"]);
        std::fs::write(dir.path().join("lib.rs"), "pub fn fix() {}\n").unwrap();
        git_cmd(&["add", "lib.rs"]);
        git_cmd(&["commit", "-m", "fix"]);
        store
            .update(|d| {
                let t = &mut d.tickets[0];
                t.title = "Fix boundary".into();
                t.base = "main".into();
                t.branch = "feat/t-1".into();
                t.worktree = Some(dir.path().into());
                t.pr_number = None;
                Ok(())
            })
            .unwrap();
        let gh = FakeBackend::new(vec![Ok(Some(crate::github::PrStatus {
            number: 7,
            url: "https://example.invalid/pr/7".into(),
            state: crate::github::PrState::Open,
            merged_at: None,
            base: "main".into(),
            head: "feat/t-1".into(),
            head_oid: None,
            checks: crate::github::Checks::Pending,
            review: crate::github::ReviewDecision::None,
            comment_ids: vec![],
        }))]);
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let t = open(
            &store,
            &gh,
            &gate(dir.path()),
            &cfg,
            "T-1",
            "Fix boundary",
            &["Validate input".into()],
            &["cargo test".into()],
            "2",
        )
        .unwrap();
        assert_eq!(t.state, TicketState::CiWait);
        assert_eq!(t.pr_number, Some(7));
        assert!(
            gh.calls
                .borrow()
                .iter()
                .any(|c| c == "reviewers 7 [\"org/team\", \"reviewer\"]")
        );
    }
    fn ready_fixture() -> (tempfile::TempDir, Store) {
        let (dir, store) = fixture(TicketState::ReadyForPr);
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "base",
            ],
            vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        ] {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::fs::write(dir.path().join("CODEOWNERS"), "* @reviewer\n").unwrap();
        // HEAD differs from base so ownership is exercised through an actual changed path.
        std::fs::write(dir.path().join("lib.rs"), "pub fn fixed() {}\n").unwrap();
        for args in [
            vec!["add", "lib.rs"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "fix",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(dir.path())
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        store
            .update(|d| {
                let t = &mut d.tickets[0];
                t.title = "Fix boundary".into();
                t.base = "main".into();
                t.branch = "feat/t-1".into();
                t.worktree = Some(dir.path().into());
                t.pr_number = None;
                Ok(())
            })
            .unwrap();
        (dir, store)
    }

    fn pr_status() -> crate::github::PrStatus {
        crate::github::PrStatus {
            number: 7,
            url: "https://example.invalid/pr/7".into(),
            state: crate::github::PrState::Open,
            merged_at: None,
            base: "main".into(),
            head: "feat/t-1".into(),
            head_oid: None,
            checks: crate::github::Checks::Pending,
            review: crate::github::ReviewDecision::None,
            comment_ids: vec![],
        }
    }

    struct ReviewerFailure {
        gh: FakeBackend,
        fail: std::cell::Cell<bool>,
        requested: Vec<String>,
    }
    impl PrBackend for ReviewerFailure {
        fn requested_reviewers(&self, _: &Gate, _: u64) -> Result<Vec<String>, ShepherdError> {
            Ok(self.requested.clone())
        }

        fn view(&self, p: &Path, b: &str) -> Result<Option<PrStatus>, ShepherdError> {
            self.gh.view(p, b)
        }
        fn push(&self, g: &Gate) -> Result<(), ShepherdError> {
            self.gh.push(g)
        }
        fn create(
            &self,
            g: &Gate,
            b: &str,
            t: &str,
            body: &str,
        ) -> Result<PrStatus, ShepherdError> {
            self.gh.create(g, b, t, body)
        }
        fn author(&self, p: &Path) -> Result<String, ShepherdError> {
            self.gh.author(p)
        }
        fn reply(&self, g: &Gate, n: u64, c: Option<u64>, b: &str) -> Result<(), ShepherdError> {
            self.gh.reply(g, n, c, b)
        }
        fn add_reviewers(&self, g: &Gate, n: u64, r: &[String]) -> Result<(), ShepherdError> {
            self.gh.add_reviewers(g, n, r)?;
            if self.fail.get() {
                Err(ShepherdError::GithubUnavailable(
                    "review request failed".into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn reviewer_failure_preserves_pr_for_retry() {
        let (dir, store) = ready_fixture();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let gh = ReviewerFailure {
            gh: FakeBackend::new(vec![Ok(Some(pr_status()))]),
            fail: std::cell::Cell::new(true),
            requested: vec![],
        };
        assert!(
            open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Fix boundary",
                &["Check input".into()],
                &["cargo test".into()],
                "2"
            )
            .is_err()
        );
        let t = &store.read().unwrap().tickets[0];
        assert_eq!(t.pr_number, Some(7));
        assert_eq!(t.state, TicketState::CiWait);
        assert_eq!(t.pr_reviewers_pending, ["reviewer"]);
        gh.fail.set(false);
        gh.gh.views.borrow_mut().push_back(Ok(Some(pr_status())));
        let t = open(
            &store,
            &gh,
            &gate(dir.path()),
            &cfg,
            "T-1",
            "Fix boundary",
            &["Check input".into()],
            &["cargo test".into()],
            "3",
        )
        .unwrap();
        assert!(t.pr_reviewers_pending.is_empty());
        assert_eq!(t.history.len(), 1);
        assert!(
            !gh.gh
                .calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("create "))
        );
    }

    #[test]
    fn create_read_failure_adopts_existing_pr() {
        let (dir, store) = ready_fixture();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let gh = FakeBackend::new(vec![
            Ok(None),
            Err(ShepherdError::GithubUnavailable(
                "read after create failed".into(),
            )),
        ]);
        assert!(
            open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Fix boundary",
                &["Check input".into()],
                &["cargo test".into()],
                "2"
            )
            .is_err()
        );
        assert_eq!(
            store.read().unwrap().tickets[0].state,
            TicketState::ReadyForPr
        );
        gh.views.borrow_mut().push_back(Ok(Some(pr_status())));
        let t = open(
            &store,
            &gh,
            &gate(dir.path()),
            &cfg,
            "T-1",
            "Fix boundary",
            &["Check input".into()],
            &["cargo test".into()],
            "3",
        )
        .unwrap();
        assert_eq!(t.pr_number, Some(7));
        assert_eq!(
            gh.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("create "))
                .count(),
            1
        );
    }

    #[test]
    fn symlink_codeowners_is_rejected_before_read_or_github() {
        use std::os::unix::fs::symlink;
        for ancestor in [false, true] {
            let (dir, store) = ready_fixture();
            std::fs::remove_file(dir.path().join("CODEOWNERS")).unwrap();
            let target = dir.path().join("external");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("CODEOWNERS"), "* @unexpected").unwrap();
            if ancestor {
                symlink(&target, dir.path().join(".github")).unwrap();
            } else {
                symlink(target.join("CODEOWNERS"), dir.path().join("CODEOWNERS")).unwrap();
            }
            let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
            let gh = FakeBackend::new(vec![]);
            assert_eq!(
                open(
                    &store,
                    &gh,
                    &gate(dir.path()),
                    &cfg,
                    "T-1",
                    "Fix boundary",
                    &["Check input".into()],
                    &["cargo test".into()],
                    "2"
                )
                .unwrap_err()
                .code(),
                "policy_denied"
            );
            assert!(gh.calls.borrow().is_empty());
        }
    }
    #[test]
    fn already_requested_owners_are_not_requested_twice() {
        let (dir, store) = ready_fixture();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let gh = ReviewerFailure {
            gh: FakeBackend::new(vec![Ok(Some(pr_status()))]),
            fail: std::cell::Cell::new(true),
            requested: vec!["REVIEWER".into()],
        };
        let t = open(
            &store,
            &gh,
            &gate(dir.path()),
            &cfg,
            "T-1",
            "Fix boundary",
            &["Check input".into()],
            &["cargo test".into()],
            "2",
        )
        .unwrap();
        assert!(t.pr_reviewers_pending.is_empty());
        assert!(
            !gh.gh
                .calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("reviewers "))
        );
    }

    #[test]
    fn pending_reviewers_recover_after_ci_handoff() {
        let (dir, store) = ready_fixture();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let gh = ReviewerFailure {
            gh: FakeBackend::new(vec![Ok(Some(pr_status()))]),
            fail: std::cell::Cell::new(true),
            requested: vec![],
        };
        assert!(
            open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Fix boundary",
                &["Check input".into()],
                &["cargo test".into()],
                "2"
            )
            .is_err()
        );
        store
            .update(|d| {
                super::super::state::transition(&mut d.tickets[0], TicketState::PrOpen, None, "3")?;
                Ok(())
            })
            .unwrap();
        gh.fail.set(false);
        gh.gh.views.borrow_mut().push_back(Ok(Some(pr_status())));
        let t = open(
            &store,
            &gh,
            &gate(dir.path()),
            &cfg,
            "T-1",
            "Fix boundary",
            &["Check input".into()],
            &["cargo test".into()],
            "4",
        )
        .unwrap();
        assert_eq!(t.state, TicketState::PrOpen);
        assert_eq!(t.history.len(), 2);
        assert!(t.pr_reviewers_pending.is_empty());
    }

    #[test]
    fn zero_pr_number_rejected_before_any_write() {
        let (dir, store) = ready_fixture();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let mut status = pr_status();
        status.number = 0;
        let gh = FakeBackend::new(vec![Ok(Some(status))]);
        assert!(
            open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Fix boundary",
                &["Check input".into()],
                &["cargo test".into()],
                "2"
            )
            .is_err()
        );
        assert!(!gh.calls.borrow().iter().any(|c| c.starts_with("push ")
            || c.starts_with("create ")
            || c.starts_with("reviewers ")));
        assert_eq!(
            store.read().unwrap().tickets[0].state,
            TicketState::ReadyForPr
        );
    }
}
