//! Startup instructions for the shepherd and its flock.
use crate::{roles::Resolved, store::StoreData};
use std::path::Path;

pub fn lead_prompt(
    bin: &Path,
    project_root: &Path,
    resolved: &Resolved,
    data: &StoreData,
) -> String {
    let command = crate::gate::shim::shell_quote(bin);
    let roles = resolved
        .roles
        .iter()
        .map(|role| {
            format!(
                "- {}: {}; runners: {}; worktree: {:?}; stage: {}{}",
                role.name,
                role.description,
                role.runners.join(", "),
                role.worktree,
                role.stage.as_deref().unwrap_or("pipeline default"),
                role.prompt
                    .as_ref()
                    .map(|p| format!("; extra instructions: {p}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tickets = data
        .tickets
        .iter()
        .map(|ticket| {
            format!(
                "- {}: {:?}{}{}",
                ticket.id,
                ticket.state,
                ticket
                    .blocked_reason
                    .map(|reason| format!(" ({reason:?})"))
                    .unwrap_or_default(),
                ticket
                    .protection_warnings
                    .iter()
                    .map(|warning| format!(
                        "\n  {}",
                        crate::github::protection::warning_line(project_root, warning)
                    ))
                    .collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"You are the shepherd for project {root}. Only you talk to the user. Report progress and blockers by ticket ID.
You must never edit repository code yourself. Delegate all code changes through {command} agent spawn. You may write task-description and ticket-intake metadata files for these CLI commands.
You must never merge PRs, approve PRs, force-push, or push protected branches ({protected}). Flock agents never push, open PRs, or call GitHub. Use the gated ticket PR commands after QA passes.
Never read .env*, Credentials/, *.pem, *.key, or other secret files. Findings use High (Blocker) | Mid | Low | Wish.

Use only the public CLI to orchestrate state. Available commands:
{command} ticket add|list|show|cancel|integrate
{command} ticket dispatch
{command} ticket transition <id> --to <state> [--reason <reason>]
{command} agent spawn --ticket <id> --role <role> --task-file <path> [--unit <n>]
{command} agent list|status|cancel
{command} status
Inspect command --help for exact arguments. Never edit state files directly. Agent report commands carry the inherited project and state-directory identity.

Honor explicit role stage mappings (review maps to reviewing). At each stage run every configured role mapped to that stage, along with its required built-in role.
For an ad-hoc request with no ticket ID or source, treat the request as a ticket. Derive a short imperative title and choose --type feat|fix|refactor|docs from the request. Write the user's full request verbatim into the --body-file, draft concrete, testable acceptance criteria from that request, and pass the criteria with --acceptance-file. Run ticket add with --title, --body-file, --acceptance-file, and --type; omit --id so the title slug becomes the generated ID. Use the actual ID returned by ticket add, including any collision suffix. Tell the user the generated ID and acceptance criteria in one short message, then run ticket dispatch immediately without waiting for confirmation. The run goes straight through. Only ask the user first if the request is too ambiguous to write any acceptance criteria. Several distinct tasks in one message become separate tickets, each retaining the full request verbatim in its body file and criteria scoped to that task.
For a request that names a ticket ID, use ticket fetch <id> when a ticket source is configured, then add the fetched ticket with --id. Otherwise use the named ID with --id and the supplied request text; do not ask for an ID or acceptance criteria that you can derive from that request. If only an ID is supplied and no source is configured, ask for the ticket text because no acceptance criteria can be derived.
For a new ticket: add it with title, body, and acceptance criteria, then dispatch the queue. Dispatch creates a ticket worktree and launches planner. Read the planner's report using agent status; its summary is the plan. Use researching and a researcher only when the ticket needs investigation, otherwise transition planning to building. Write a task file with the complete ticket context and plan, then spawn coder. Once all coders report, integrate child worktrees before transitioning to reviewing and spawning reviewer. Read every report before advancing. Address High (Blocker) findings and failed checks by transitioning reviewing or qa back to building, then delegate a coder fix; the configured revision cap is {cap}. Once review has no blockers, transition to qa and spawn qa. After QA reports all acceptance checks passed, transition to ready_for_pr, prepare the summary, changes, and testing files, then run ticket pr to push and open the PR. CI and comment pickup are driven by the watcher. When red CI returns the ticket to building, inspect the stored ci_failure_task and its check names and URLs, then dispatch the coder, review, and QA again. Never skip review or QA. Reported agents no longer consume agent capacity. Use ticket dispatch whenever a ticket frees a slot; revising tickets have priority over queued tickets. Use agent cancel for a stuck owned agent; keep its worktree and commits.
For a blocked ticket, inspect history and return only to its recorded prior state using ticket transition before assigning a resolver. For child conflicts, give the resolver both recorded ranges and have it merge the child branches locally into the ticket branch with any conflicts resolved; then rerun ticket integrate to recognize contained commits and clean up.
When supervision sends a dialog, answer from the ticket context or deny it. Never approve a gate-forbidden action. Do not prompt a blocked agent; safe dialog keys go through supervision. Unresolved blocks should be escalated to the user.

PR commands and comment triage:
{pr_commands}
Call these commands through {command}. After ticket revise, run ticket dispatch until the revision returns to ci_wait; every revision runs coder, reviewer, and QA. A later dispatch advances a revision when its agent has reported. Never bypass these stages. When the watcher hands off pr_open, retire any remaining reported flock agents and keep the worktree and PR.

Configured role catalog:
{roles}

Current tickets:
{tickets}
"#,
        root = project_root.display(),
        protected = resolved.shepherd.protected_branches.join(", "),
        cap = resolved.shepherd.max_revision_rounds,
        pr_commands = crate::ticket::pr::LEAD_PR_COMMANDS,
    )
}

pub fn role_prompt(
    bin: &Path,
    ticket: &crate::store::Ticket,
    role: &crate::roles::Role,
    task: &str,
    agent_name: &str,
    harness: &dyn crate::harness::Harness,
) -> String {
    let worktree = ticket.worktree.as_deref().unwrap_or(Path::new("."));
    let reporting = report_instructions(bin, worktree, agent_name, "{{task_id}}");
    let acceptance = ticket
        .acceptance
        .iter()
        .map(|a| format!("- {a}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"You are the {role_name} flock agent for ticket {id}: {title}.
Assigned worktree: {worktree}
Assigned branch: {branch}
Stay on the assigned branch and worktree. Never switch branches and never write outside it. Never read .env*, Credentials/, *.pem, *.key, or other secret files. Never push, open PRs, or call GitHub; the shepherd owns those actions. Do not spawn more shepherd agents. You may {hint} inside your harness when useful.
Only the shepherd talks to the user. Submit your result to the shepherd through the report command below. Label findings High (Blocker) | Mid | Low | Wish. The JSON severity values are high, mid, low, wish.
Code changes must be test-first, scoped to this task, and committed on the assigned branch. Use conventional commits, type(scope): imperative subject under 50 characters, with a body explaining why. Do not add AI attribution or Co-Authored-By trailers. Reviewers and QA should inspect and test the complete ticket diff and report concrete evidence; delegate code fixes back through the shepherd.

Ticket body:
{body}
Acceptance criteria:
{acceptance}
Plan:
{plan}
Research:
{research}
Role description:
{description}
Assigned task:
{task}
Role instructions:
{extra}

{reporting}
"#,
        role_name = role.name,
        id = ticket.id,
        title = ticket.title,
        worktree = worktree.display(),
        branch = ticket.branch,
        hint = harness.subagent_hint(),
        body = ticket.body,
        plan = ticket.plan.as_deref().unwrap_or("Not yet available"),
        research = ticket.research.join("\n"),
        description = role.description,
        extra = role
            .prompt
            .as_deref()
            .unwrap_or("Follow the assigned role description and ticket acceptance criteria."),
    )
}

/// Shared by initial role tasks and PR revision tasks. Placeholders are bound
/// by launch before the prompt can reach a harness.
pub fn report_instructions(bin: &Path, worktree: &Path, agent_name: &str, task_id: &str) -> String {
    let report = worktree.join(format!(".shepherd-report-{agent_name}-{task_id}.json"));
    let command = format!(
        "{} agent report --agent {} --task-id {} --report-file {}",
        crate::gate::shim::shell_quote(bin),
        crate::gate::shim::shell_quote(Path::new(agent_name)),
        crate::gate::shim::shell_quote(Path::new(task_id)),
        crate::gate::shim::shell_quote(&report)
    );
    format!(
        r#"Assignment task ID: {task_id}
When finished, write {report_path} inside this worktree using this JSON schema:
{{"task_id":"{task_id}","status":"completed|blocked|failed","summary":"what happened and why","tests":["command and result"],"changed_paths":["relative/path"],"blockers":["reason"],"findings":[{{"severity":"high|mid|low|wish","text":"concrete finding","location":"path:line or null"}}],"commit_sha":"full commit SHA or null"}}
Use actual completed, blocked, or failed as status; use JSON null for absent location or commit_sha. Empty arrays are valid. Include the plan in the planner summary and research findings in the researcher summary. Completed must mean this role's checks passed; failed checks or unresolved blockers must be reported explicitly.
Submit exactly:
{command}
After a successful report, remove only your report file with rm -- {quoted_report}. Never commit report files. Then stop.
"#,
        report_path = report.display(),
        quoted_report = crate::gate::shim::shell_quote(&report)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_prompt_has_triage_rules() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let resolved = crate::roles::resolve(&cfg).unwrap();
        let mut data = crate::store::Store::open(&dir.path().join("state"), dir.path())
            .unwrap()
            .read()
            .unwrap();
        data.tickets.push(crate::store::Ticket {
            id: "T-4".into(),
            state: crate::store::TicketState::PrOpen,
            protection_warnings: vec![crate::github::protection::ProtectionWarning {
                branch: "trunk".into(),
                reason: "unprotected".into(),
            }],
            ..Default::default()
        });
        let prompt = lead_prompt(
            Path::new("/tmp/bin/herdr-shepherd"),
            dir.path(),
            &resolved,
            &data,
        );
        for required in [
            "ticket reply",
            "ticket revise",
            "never resolve",
            "actual code",
            "reproduce",
            "ticket fetch <id>",
            "paste the ticket",
            "trunk has no branch protection",
            "user's voice",
        ] {
            assert!(prompt.contains(required), "missing {required}");
        }
        assert!(!prompt.contains("PR commands arrive in a later phase"));
        assert!(!prompt.contains("ready_for_pr and stop"));
    }

    #[test]
    fn lead_prompt_includes_stage_mapped_roles() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let mut resolved = crate::roles::resolve(&cfg).unwrap();
        resolved.roles.push(crate::roles::Role {
            name: "security-reviewer".into(),
            stage: Some("review".into()),
            ..Default::default()
        });
        let data = crate::store::Store::open(&dir.path().join("state"), dir.path())
            .unwrap()
            .read()
            .unwrap();
        let prompt = lead_prompt(
            Path::new("/tmp/bin/herdr-shepherd"),
            dir.path(),
            &resolved,
            &data,
        );
        assert!(prompt.contains("stage: review"));
        assert!(prompt.contains("every configured role"));
    }

    #[test]
    fn lead_prompt_handles_adhoc_requests_without_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let resolved = crate::roles::resolve(&cfg).unwrap();
        let data = crate::store::Store::open(&dir.path().join("state"), dir.path())
            .unwrap()
            .read()
            .unwrap();
        let prompt = lead_prompt(Path::new("shepherd"), dir.path(), &resolved, &data);
        for required in [
            "ad-hoc request with no ticket ID or source",
            "treat the request as a ticket",
            "short imperative title",
            "--type feat|fix|refactor|docs",
            "user's full request verbatim into the --body-file",
            "concrete, testable acceptance criteria",
            "pass the criteria with --acceptance-file",
            "omit --id so the title slug becomes the generated ID",
            "actual ID returned by ticket add, including any collision suffix",
            "generated ID and acceptance criteria in one short message",
            "run ticket dispatch immediately without waiting for confirmation",
            "Only ask the user first if the request is too ambiguous to write any acceptance criteria",
            "Several distinct tasks in one message become separate tickets",
            "each retaining the full request verbatim in its body file",
            "ticket fetch <id> when a ticket source is configured",
            "Otherwise use the named ID with --id and the supplied request text",
        ] {
            assert!(prompt.contains(required), "missing {required}");
        }
    }

    #[test]
    fn lead_prompt_preserves_safety_lines() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(&dir.path().join("cfg"), None).unwrap();
        let resolved = crate::roles::resolve(&cfg).unwrap();
        let data = crate::store::Store::open(&dir.path().join("state"), dir.path())
            .unwrap()
            .read()
            .unwrap();
        let prompt = lead_prompt(Path::new("shepherd"), dir.path(), &resolved, &data);
        let command = crate::gate::shim::shell_quote(Path::new("shepherd"));
        let protected = resolved.shepherd.protected_branches.join(", ");
        for line in [
            format!("You must never edit repository code yourself. Delegate all code changes through {command} agent spawn. You may write task-description and ticket-intake metadata files for these CLI commands."),
            format!("You must never merge PRs, approve PRs, force-push, or push protected branches ({protected}). Flock agents never push, open PRs, or call GitHub. Use the gated ticket PR commands after QA passes."),
            "Never read .env*, Credentials/, *.pem, *.key, or other secret files. Findings use High (Blocker) | Mid | Low | Wish.".into(),
            "Inspect command --help for exact arguments. Never edit state files directly. Agent report commands carry the inherited project and state-directory identity.".into(),
            "When supervision sends a dialog, answer from the ticket context or deny it. Never approve a gate-forbidden action. Do not prompt a blocked agent; safe dialog keys go through supervision. Unresolved blocks should be escalated to the user.".into(),
        ] {
            assert!(prompt.lines().any(|actual| actual == line), "missing safety line: {line}");
        }
        assert!(prompt.contains("Never skip review or QA."));
        assert!(prompt.contains("Never bypass these stages."));
        assert!(
            prompt.contains("never resolve threads. Never request changes, approve, or merge.")
        );
    }

    #[test]
    fn role_prompt_contains_report_command_and_secrets_rule() {
        let ticket = crate::store::Ticket {
            id: "T-1".into(),
            title: "Hello".into(),
            body: "Add hello".into(),
            branch: "feat/t-1".into(),
            worktree: Some("/tmp/ticket".into()),
            acceptance: vec!["Print hello".into()],
            ..Default::default()
        };
        let role = crate::roles::Role {
            name: "coder".into(),
            ..Default::default()
        };
        let harness = crate::harness::harness_for("codex").unwrap();
        let prompt = role_prompt(
            Path::new("/tmp/bin/herdr-shepherd"),
            &ticket,
            &role,
            "Implement hello",
            "sh-123abc-coder-1",
            harness.as_ref(),
        );
        for text in [
            "agent report --agent",
            "sh-123abc-coder-1",
            "--report-file",
            ".env*",
            "Credentials/",
            "*.pem",
            "*.key",
            "Never push",
            "never write outside",
            "feat/t-1",
            "/tmp/ticket",
            "Print hello",
            "High (Blocker)",
            "multi_agent",
            "changed_paths",
            "commit_sha",
        ] {
            assert!(prompt.contains(text), "missing {text}");
        }
    }
    #[test]
    fn lead_prompt_contains_rules() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        let resolved = crate::roles::resolve(&cfg).unwrap();
        let data = crate::store::Store::open(&dir.path().join("state"), dir.path())
            .unwrap()
            .read()
            .unwrap();
        let prompt = lead_prompt(
            std::path::Path::new("/tmp/bin/herdr-shepherd"),
            dir.path(),
            &resolved,
            &data,
        );
        for text in [
            "never edit",
            "agent spawn",
            "High (Blocker)",
            "never merge",
            "ticket transition",
            "ticket dispatch",
            "ready_for_pr",
        ] {
            assert!(prompt.contains(text), "missing {text}");
        }
        for role in &resolved.roles {
            assert!(prompt.contains(&role.name));
        }
    }
}
