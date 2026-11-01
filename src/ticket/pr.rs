//! Lead-facing pull request commands.

#[cfg(test)]
use std::path::Path;

use crate::{
    config::Config,
    error::ShepherdError,
    gate::{Gate, check_push, parse_push_args},
    github::{CommentKind, PrBackend, PrStatus, codeowners},
    store::{CommentRecord, Store, Ticket, TicketState},
};

pub const LEAD_PR_COMMANDS: &str = r#"
Use ticket pr <id> --summary-file <p> --changes-file <p> --testing-file <p>
when ready_for_pr to push and open a ready-for-review PR.
Inspect ticket show <id> for append-only findings and their zero-based indices.
Optionally add --finding-dispositions-file <p> with strict JSON:
{"ticket_id":"<id>","findings":[{"index":0,"status":"open","reason":"Still reproducible"}]}.
Use open, deferred, or resolved explicitly; reason is optional text.
Open/deferred findings of every severity stay visible as current issues.
Resolved and omitted entries stay visible as history; omitted means unclassified.
Missing or empty metadata never resolves findings. Never remove stored evidence.
Raw reports remain available through agent status <name>.
Wrong ticket IDs, duplicate/out-of-range indices, unknown fields/statuses and
malformed metadata are rejected. Publication and QA gates still apply.
Use ticket comments <id> --new to inspect comments awaiting triage.
Check each comment against the actual code and reproduce the issue where possible.
If a comment is not relevant, use ticket reply <id> --comment <cid> --body-file <p>
with the reason and evidence. For a legitimate issue, use ticket revise <id>
--comments <cid,cid> --task-file <p> to send the fix through the flock.
Use --kind review|issue|review-body when numeric comment ids collide.
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

fn selected(ticket: &Ticket, id: u64, kind: Option<CommentKind>) -> Result<usize, ShepherdError> {
    let found: Vec<_> = ticket
        .comments
        .iter()
        .enumerate()
        .filter(|(_, c)| c.id == id && kind.is_none_or(|kind| c.comment_kind() == kind))
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
    open_with_dispositions(
        store, gh, gate, cfg, id, summary, changes, testing, None, now,
    )
}

/// Open a PR with explicit publication dispositions, validated under the dispatch lock.
#[allow(clippy::too_many_arguments)]
pub fn open_with_dispositions(
    store: &Store,
    gh: &dyn PrBackend,
    gate: &Gate,
    cfg: &Config,
    id: &str,
    summary: &str,
    changes: &[String],
    testing: &[String],
    dispositions: Option<&super::pr_body::FindingDispositions>,
    now: &str,
) -> Result<Ticket, ShepherdError> {
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let cap = crate::roles::resolve(cfg)?.shepherd.max_revision_rounds;
    let mut snapshot = store.read()?;
    let observed = ticket(&snapshot, id)?.clone();
    let (mut result, opened) = (|| {
        let data = &mut snapshot;
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
        let body = super::pr_body::build_body_with_dispositions(
            t,
            summary,
            changes,
            testing,
            dispositions,
        )?;
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
        let tested_sha = t.qa_commit_sha.as_deref().ok_or_else(|| {
            ShepherdError::Usage(
                "completed QA must report its commit SHA before opening a PR".into(),
            )
        })?;
        let head = crate::gate::transport::PushTransport::open(&gate.state_dir, &gate.worktree)?
            .verified_ticket_head(&t.branch)?;
        if head != tested_sha {
            return Err(ShepherdError::Conflict {
                unit: 0,
                range: "ticket HEAD differs from the QA-tested commit".into(),
            });
        }
        let reviewers = reviewers(gh, gate, &t.base)?;
        let existing = gh.view(&gate.worktree, &t.branch)?;
        if let Some(status) = &existing {
            validate_status(t, status)?;
        }
        gh.push_expected(gate, tested_sha)?;
        let status = match existing {
            Some(status) => status,
            None => gh.create(gate, &t.base, &title, &body)?,
        };
        validate_status(t, &status)?;
        let pushed_sha = tested_sha.to_owned();
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        t.pr_number = Some(status.number);
        t.ci_head_sha = Some(pushed_sha);
        t.pr_reviewers_pending = reviewers;
        super::state::transition_with_limit(t, TicketState::CiWait, None, now, cap)?;
        Ok((t.clone(), true))
    })()?;
    let unchanged = |current: &Ticket| {
        current.history.len() == observed.history.len()
            && current.state == observed.state
            && current.pr_number == observed.pr_number
            && current.updated_at == observed.updated_at
    };
    if opened {
        result = store.update(|data| {
            let current = data
                .tickets
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
            if !unchanged(current) {
                return Err(ShepherdError::Usage(
                    "ticket changed while opening PR".into(),
                ));
            }
            current.pr_number = result.pr_number;
            current.ci_head_sha = result.ci_head_sha.clone();
            current.pr_reviewers_pending = result.pr_reviewers_pending.clone();
            current.state = result.state;
            current.updated_at = result.updated_at.clone();
            current.history = result.history.clone();
            Ok(current.clone())
        })?;
    } else {
        let current = store
            .read()?
            .tickets
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        if !unchanged(&current) {
            return Err(ShepherdError::Usage(
                "ticket changed while retrying PR reviewers".into(),
            ));
        }
        result = current;
    }
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

/// Resolve owners from the verified base branch, never the ticket's editable copy.
pub fn reviewers(
    gh: &dyn PrBackend,
    gate: &Gate,
    base: &str,
) -> Result<Vec<String>, ShepherdError> {
    let transport = crate::gate::transport::PushTransport::open_with_options(
        &gate.state_dir,
        &gate.worktree,
        crate::gate::transport::TransportOptions {
            allow_local_for_tests: cfg!(test),
        },
    )?;
    let head = transport.verified_ticket_head(&gate.ticket_branch)?;
    let base_sha = transport.fetch_branch(base)?;
    let mut rules = Vec::new();
    for path in [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"] {
        let entry = transport.execute(&["ls-tree", &base_sha, "--", path])?;
        if entry.trim().is_empty() {
            continue;
        }
        let (metadata, _) = entry
            .trim_end()
            .split_once('\t')
            .ok_or_else(|| ShepherdError::PolicyDenied("invalid CODEOWNERS tree entry".into()))?;
        let parts: Vec<_> = metadata.split_whitespace().collect();
        if parts.len() != 3 || !matches!(parts[0], "100644" | "100755") || parts[1] != "blob" {
            return Err(ShepherdError::PolicyDenied(
                "base CODEOWNERS must be a regular blob".into(),
            ));
        }
        rules = codeowners::parse(&transport.execute(&["cat-file", "blob", parts[2]])?);
        break;
    }
    if rules.is_empty() {
        return Ok(vec![]);
    }
    let changed = transport.execute(&[
        "diff",
        "--name-only",
        "-z",
        &format!("{base_sha}...{head}"),
        "--",
    ])?;
    let paths = changed
        .split('\0')
        .filter(|path| !path.is_empty())
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
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let snapshot = store.read()?;
    let t = ticket(&snapshot, id)?;
    let number = t
        .pr_number
        .ok_or_else(|| ShepherdError::Usage("ticket has no PR number".into()))?;
    let worktree = t
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    let fresh = gh.comments(worktree, number)?;
    store.update(|data| {
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .expect("validated ticket");
        for c in fresh {
            if t.posted_replies.iter().any(|posted| {
                (posted.review == (c.comment_kind() == CommentKind::Review)
                    && c.comment_kind() != CommentKind::ReviewBody
                    && posted.id == Some(c.id))
                    || (posted.author.eq_ignore_ascii_case(&c.author) && posted.body == c.body)
            }) {
                continue;
            }
            if !t
                .comments
                .iter()
                .any(|stored| stored.id == c.id && stored.comment_kind() == c.comment_kind())
            {
                t.comments.push(CommentRecord {
                    kind: Some(c.comment_kind()),
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
    reply_with_kind(store, gh, gate, id, cid, body, None)
}

#[allow(clippy::too_many_arguments)]
pub fn reply_with_kind(
    store: &Store,
    gh: &dyn PrBackend,
    gate: &Gate,
    id: &str,
    cid: u64,
    body: &str,
    kind: Option<CommentKind>,
) -> Result<Ticket, ShepherdError> {
    validate_body(body)?;
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let snapshot = store.read()?;
    let t = ticket(&snapshot, id)?;
    let number = require_pr(t)?;
    if t.state != TicketState::PrOpen {
        return Err(ShepherdError::Usage("triage requires pr_open".into()));
    }
    let index = selected(t, cid, kind)?;
    if t.comments[index].triaged {
        return Err(ShepherdError::Usage("comment already triaged".into()));
    }
    authorize(t, gate)?;
    let pending = crate::store::PostedReply {
        id: None,
        author: gh.author(&gate.worktree)?,
        body: body.into(),
        review: t.comments[index].review,
    };
    let intent = record_reply_intent(store, t, pending)?;
    let reply_id = gh.reply_id(gate, number, t.comments[index].review.then_some(cid), body)?;
    store.update(|data| {
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        t.posted_replies[intent].id = reply_id;
        let index = selected(t, cid, kind)?;
        t.comments[index].triaged = true;
        t.comments[index].verdict = Some("declined".into());
        Ok(t.clone())
    })
}

fn record_reply_intent(
    store: &Store,
    observed: &Ticket,
    posted: crate::store::PostedReply,
) -> Result<usize, ShepherdError> {
    store.update(|data| {
        let current = data
            .tickets
            .iter_mut()
            .find(|t| t.id == observed.id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {}", observed.id)))?;
        if current.state != observed.state
            || current.history.len() != observed.history.len()
            || current.pr_number != observed.pr_number
        {
            return Err(ShepherdError::Usage("ticket changed before reply".into()));
        }
        if let Some(index) = current.posted_replies.iter().position(|old| old == &posted) {
            return Ok(index);
        }
        current.posted_replies.push(posted);
        Ok(current.posted_replies.len() - 1)
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
    revise_with_kind(store, id, cids, task, now, cap, None)
}

#[allow(clippy::too_many_arguments)]
pub fn revise_with_kind(
    store: &Store,
    id: &str,
    cids: &[u64],
    task: &str,
    now: &str,
    cap: u32,
    kind: Option<CommentKind>,
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
            let index = selected(t, *cid, kind)?;
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
    ack_with_kind(store, gh, gate, id, cid, body, None)
}

#[allow(clippy::too_many_arguments)]
pub fn ack_with_kind(
    store: &Store,
    gh: &dyn PrBackend,
    gate: &Gate,
    id: &str,
    cid: u64,
    body: &str,
    kind: Option<CommentKind>,
) -> Result<Ticket, ShepherdError> {
    validate_body(body)?;
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let snapshot = store.read()?;
    let t = ticket(&snapshot, id)?;
    let number = require_pr(t)?;
    if !matches!(t.state, TicketState::CiWait | TicketState::PrOpen) {
        return Err(ShepherdError::Usage(
            "ack requires a pushed revision".into(),
        ));
    }
    let index = selected(t, cid, kind)?;
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
    let pending = crate::store::PostedReply {
        id: None,
        author: gh.author(&gate.worktree)?,
        body: body.into(),
        review: c.review,
    };
    let intent = record_reply_intent(store, t, pending)?;
    let reply_id = gh.reply_id(gate, number, c.review.then_some(cid), body)?;
    store.update(|data| {
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        t.posted_replies[intent].id = reply_id;
        let index = selected(t, cid, kind)?;
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
        for rule in [
            "ticket reply",
            "ticket revise",
            "never resolve",
            "--finding-dispositions-file",
            "ticket show",
            "agent status",
            "omitted means unclassified",
            "Never remove stored evidence",
        ] {
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
                kind: None,
            },
            crate::github::PrComment {
                id: 2,
                author: "reviewer".into(),
                path: None,
                line: None,
                body: "New".into(),
                review: false,
                kind: None,
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

    struct ReplyAfterPostError {
        gh: FakeBackend,
    }
    impl PrBackend for ReplyAfterPostError {
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
        fn add_reviewers(&self, g: &Gate, n: u64, r: &[String]) -> Result<(), ShepherdError> {
            self.gh.add_reviewers(g, n, r)
        }
        fn author(&self, p: &Path) -> Result<String, ShepherdError> {
            self.gh.author(p)
        }
        fn comments(
            &self,
            p: &Path,
            n: u64,
        ) -> Result<Vec<crate::github::PrComment>, ShepherdError> {
            self.gh.comments(p, n)
        }
        fn reply(&self, g: &Gate, n: u64, c: Option<u64>, b: &str) -> Result<(), ShepherdError> {
            self.gh.reply(g, n, c, b)
        }
        fn reply_id(
            &self,
            g: &Gate,
            n: u64,
            c: Option<u64>,
            b: &str,
        ) -> Result<Option<u64>, ShepherdError> {
            self.gh.reply_id(g, n, c, b)?;
            Err(ShepherdError::GithubUnavailable(
                "response lost after GitHub accepted reply".into(),
            ))
        }
    }

    #[test]
    fn accepted_reply_error_keeps_pending_echo_filter() {
        for acknowledge in [false, true] {
            let (dir, store) = fixture(TicketState::PrOpen);
            store
                .update(|data| {
                    let t = &mut data.tickets[0];
                    t.branch = "feat/t-1".into();
                    t.base = "main".into();
                    t.worktree = Some(dir.path().into());
                    t.comments.push(comment(1));
                    if acknowledge {
                        t.comments[0].triaged = true;
                        t.comments[0].verdict = Some("accepted".into());
                        t.comments[0].ack_sha = Some("abc123".into());
                    }
                    Ok(())
                })
                .unwrap();
            let gh = ReplyAfterPostError {
                gh: FakeBackend::new(vec![]),
            };
            let body = "Covered by the parser boundary test in abc123.";
            let result = if acknowledge {
                ack(&store, &gh, &gate(dir.path()), "T-1", 1, body)
            } else {
                reply(&store, &gh, &gate(dir.path()), "T-1", 1, body)
            };
            assert_eq!(result.unwrap_err().code(), "github_unavailable");
            let listed = comments(&store, &gh, "T-1", true).unwrap();
            assert!(
                listed
                    .iter()
                    .all(|listed| listed.comment.author != "fixture-author"),
                "accepted shepherd reply was re-ingested after a backend error"
            );
            let saved = store.read().unwrap();
            assert_eq!(saved.tickets[0].posted_replies.len(), 1);
            assert_eq!(saved.tickets[0].posted_replies[0].id, None);
            assert_eq!(saved.tickets[0].posted_replies[0].body, body);
            assert!(!saved.tickets[0].comments[0].acknowledged);
        }
    }

    #[test]
    fn own_replies_and_acks_are_not_ingested() {
        for acknowledge in [false, true] {
            let (dir, store) = fixture(TicketState::PrOpen);
            let body = "The boundary is already checked; see parser.rs:12 abc123.";
            store
                .update(|data| {
                    let ticket = &mut data.tickets[0];
                    ticket.branch = "feat/t-1".into();
                    ticket.worktree = Some(dir.path().into());
                    let mut comment = comment(1);
                    if acknowledge {
                        comment.triaged = true;
                        comment.verdict = Some("accepted".into());
                        comment.ack_sha = Some("abc123".into());
                    }
                    ticket.comments.push(comment);
                    Ok(())
                })
                .unwrap();
            let gh = FakeBackend::new(vec![]);
            if acknowledge {
                ack(&store, &gh, &gate(dir.path()), "T-1", 1, body).unwrap();
            } else {
                reply(&store, &gh, &gate(dir.path()), "T-1", 1, body).unwrap();
            }
            let saved = store.read().unwrap();
            assert!(saved.tickets[0].posted_replies[0].id.is_some());
            gh.comments.borrow_mut()[0].author = "different-author".into();
            gh.comments.borrow_mut()[0].body = "Edited after posting".into();
            gh.comments.borrow_mut().extend([
                crate::github::PrComment {
                    id: 500,
                    author: "fixture-author".into(),
                    body: body.into(),
                    path: None,
                    line: None,
                    review: true,
                    kind: None,
                },
                crate::github::PrComment {
                    id: 501,
                    author: "reviewer".into(),
                    body: body.into(),
                    path: None,
                    line: None,
                    review: true,
                    kind: None,
                },
                crate::github::PrComment {
                    id: 502,
                    author: "fixture-author".into(),
                    body: "A separate concern".into(),
                    path: None,
                    line: None,
                    review: true,
                    kind: None,
                },
            ]);
            let new = comments(&store, &gh, "T-1", true).unwrap();
            assert_eq!(
                new.iter()
                    .map(|comment| comment.comment.id)
                    .collect::<Vec<_>>(),
                [501, 502]
            );
            let saved = store.read().unwrap();
            assert_eq!(saved.tickets[0].comments.len(), 3);
        }
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
        assert_eq!(
            gh.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("reply "))
                .count(),
            2
        );
    }

    #[test]
    fn initial_and_ci_fix_push_record_the_ci_head() {
        for existing_pr in [None, Some(7)] {
            let (dir, store) = ready_fixture();
            store
                .update(|data| {
                    data.tickets[0].pr_number = existing_pr;
                    Ok(())
                })
                .unwrap();
            let expected = store.read().unwrap().tickets[0].qa_commit_sha.clone();
            let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
            let gh = FakeBackend::new(vec![Ok(Some(pr_status()))]);
            let result = open(
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
            assert_eq!(result.ci_head_sha, expected);
            assert_eq!(store.read().unwrap().tickets[0].ci_head_sha, expected);
        }
    }

    #[test]
    fn initial_and_ci_fix_push_reject_head_after_qa() {
        for existing_pr in [None, Some(7)] {
            let (dir, store) = ready_fixture();
            let tested = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap();
            let tested = String::from_utf8(tested.stdout).unwrap().trim().to_owned();
            store
                .update(|data| {
                    data.tickets[0].qa_commit_sha = Some(tested);
                    data.tickets[0].pr_number = existing_pr;
                    Ok(())
                })
                .unwrap();
            let result = std::process::Command::new("git")
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
                    "commit",
                    "--allow-empty",
                    "-m",
                    "untested change",
                ])
                .output()
                .unwrap();
            assert!(result.status.success());
            let gh = FakeBackend::new(vec![Ok(Some(pr_status()))]);
            let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
            let result = open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Fix boundary",
                &["Check input".into()],
                &["cargo test".into()],
                "2",
            );
            assert_eq!(result.unwrap_err().code(), "conflict");
            assert!(gh.calls.borrow().is_empty());
            assert_eq!(
                store.read().unwrap().tickets[0].state,
                TicketState::ReadyForPr
            );
        }
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
        snapshot_fixture(dir.path(), &store);
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
    fn snapshot_fixture(dir: &Path, store: &Store) {
        let remote = dir.join("remote.git");
        let out = std::process::Command::new("git")
            .args(["clone", "--bare", "--quiet"])
            .arg(dir)
            .arg(&remote)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let oid = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "main"])
            .output()
            .unwrap();
        let head = std::process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        store
            .update(|data| {
                data.tickets[0].qa_commit_sha =
                    Some(String::from_utf8_lossy(&head.stdout).trim().into());
                data.trust = Some(crate::store::ProjectTrust {
                    project_root: dir.into(),
                    common_git_dir: dir.join(".git"),
                    origin_url: remote.to_string_lossy().into_owned(),
                    push_url: remote.to_string_lossy().into_owned(),
                    default_branch: "main".into(),
                    config_commit: Some(String::from_utf8_lossy(&oid.stdout).trim().into()),
                    config_blob: None,
                    config_text: None,
                    trusted_at: "fixture".into(),
                    global_config: None,
                    object_format: "sha1".into(),
                });
                Ok(())
            })
            .unwrap();
    }

    fn ready_fixture() -> (tempfile::TempDir, Store) {
        let (dir, store) = fixture(TicketState::ReadyForPr);
        let git = |args: &[&str]| {
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
        git(&["init", "-b", "main"]);
        std::fs::write(dir.path().join("CODEOWNERS"), "* @reviewer\n").unwrap();
        git(&["add", "CODEOWNERS"]);
        git(&["commit", "-m", "base"]);
        git(&["switch", "-c", "feat/t-1"]);
        std::fs::write(dir.path().join("lib.rs"), "pub fn fixed() {}\n").unwrap();
        git(&["add", "lib.rs"]);
        git(&["commit", "-m", "fix"]);
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
        snapshot_fixture(dir.path(), &store);
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
        fn push_expected(&self, g: &Gate, sha: &str) -> Result<(), ShepherdError> {
            self.gh.push_expected(g, sha)
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

    struct ConcurrentPrView {
        gh: FakeBackend,
        store: Store,
        to: TicketState,
    }
    impl PrBackend for ConcurrentPrView {
        fn view(&self, p: &Path, b: &str) -> Result<Option<PrStatus>, ShepherdError> {
            let status = self.gh.view(p, b)?;
            self.store.update(|data| {
                let t = &mut data.tickets[0];
                let reason = (self.to == TicketState::Blocked)
                    .then_some(crate::store::BlockedReason::Escalated);
                super::super::state::transition(t, self.to, reason, "concurrent")?;
                Ok(())
            })?;
            Ok(status)
        }
        fn push(&self, g: &Gate) -> Result<(), ShepherdError> {
            self.gh.push(g)
        }
        fn push_expected(&self, g: &Gate, sha: &str) -> Result<(), ShepherdError> {
            self.gh.push_expected(g, sha)
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
        fn add_reviewers(&self, g: &Gate, n: u64, r: &[String]) -> Result<(), ShepherdError> {
            self.gh.add_reviewers(g, n, r)
        }
        fn author(&self, p: &Path) -> Result<String, ShepherdError> {
            self.gh.author(p)
        }
        fn reply(&self, g: &Gate, n: u64, c: Option<u64>, b: &str) -> Result<(), ShepherdError> {
            self.gh.reply(g, n, c, b)
        }
    }

    #[test]
    fn reviewer_retry_preserves_concurrent_lifecycle_change() {
        for to in [TicketState::Revising, TicketState::Blocked] {
            let (dir, store) = ready_fixture();
            let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
            let initial = ReviewerFailure {
                gh: FakeBackend::new(vec![Ok(Some(pr_status()))]),
                fail: std::cell::Cell::new(true),
                requested: vec![],
            };
            assert!(
                open(
                    &store,
                    &initial,
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
                .update(|data| {
                    super::super::state::transition(
                        &mut data.tickets[0],
                        TicketState::PrOpen,
                        None,
                        "3",
                    )?;
                    Ok(())
                })
                .unwrap();
            let before = store.read().unwrap().tickets[0].history.len();
            let gh = ConcurrentPrView {
                gh: FakeBackend::new(vec![Ok(Some(pr_status()))]),
                store: store.clone(),
                to,
            };
            let result = open(
                &store,
                &gh,
                &gate(dir.path()),
                &cfg,
                "T-1",
                "Fix boundary",
                &["Check input".into()],
                &["cargo test".into()],
                "4",
            );
            let current = store.read().unwrap().tickets[0].clone();
            assert_eq!(current.state, to, "retry overwrote concurrent state");
            assert_eq!(current.history.len(), before + 1);
            assert!(result.is_err());
            assert!(
                !gh.gh
                    .calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("reviewers "))
            );
        }
    }

    #[test]
    fn initial_open_preserves_concurrent_block() {
        let (dir, store) = ready_fixture();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let gh = ConcurrentPrView {
            gh: FakeBackend::new(vec![Ok(Some(pr_status()))]),
            store: store.clone(),
            to: TicketState::Blocked,
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
                "4"
            )
            .is_err()
        );
        let current = store.read().unwrap().tickets[0].clone();
        assert_eq!(current.state, TicketState::Blocked);
        assert_eq!(current.history.len(), 1);
        assert_eq!(current.history[0].to, TicketState::Blocked);
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
    fn worktree_codeowners_is_ignored() {
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
            let gh = FakeBackend::new(vec![Ok(Some(pr_status()))]);
            open(
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
            assert!(
                gh.calls
                    .borrow()
                    .iter()
                    .any(|call| call == "reviewers 7 [\"reviewer\"]")
            );
            assert!(
                !gh.calls
                    .borrow()
                    .iter()
                    .any(|call| call.contains("unexpected"))
            );
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
