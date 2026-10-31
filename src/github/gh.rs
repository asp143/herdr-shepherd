use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::{Checks, PrBackend, PrState, PrStatus, ReviewDecision};
use crate::{
    error::ShepherdError,
    gate::{BranchPolicy, Gate},
};

#[derive(Default)]
pub struct GhBackend {
    state_dir: Option<std::path::PathBuf>,
}

// Preserve the default-backend value used by existing embedding callers.
#[allow(non_upper_case_globals)]
pub const GhBackend: GhBackend = GhBackend { state_dir: None };

fn unavailable(error: impl std::fmt::Display) -> ShepherdError {
    ShepherdError::GithubUnavailable(error.to_string())
}

impl GhBackend {
    pub fn with_state(state_dir: &Path) -> Self {
        Self {
            state_dir: Some(state_dir.into()),
        }
    }
    fn state_dir(&self) -> std::path::PathBuf {
        self.state_dir
            .clone()
            .unwrap_or_else(crate::gate::state_dir)
    }
    fn read_gate(&self, worktree: &Path) -> Result<Gate, ShepherdError> {
        Ok(Gate {
            policy: BranchPolicy::new(&[], None, None)?,
            ticket_branch: String::new(),
            worktree: worktree.into(),
            state_dir: self.state_dir(),
            config_dir: crate::config::resolve_config_dir(None),
        })
    }

    pub fn create_args(base: &str, title: &str, body_file: &str) -> Vec<String> {
        [
            "pr",
            "create",
            "--base",
            base,
            "--title",
            title,
            "--body-file",
            body_file,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// GitHub may have already requested owners automatically.
    pub fn requested_reviewers(
        &self,
        gate: &Gate,
        number: u64,
    ) -> Result<Vec<String>, ShepherdError> {
        let text = gate
            .gh(
                &[
                    "pr",
                    "view",
                    &number.to_string(),
                    "--json",
                    "reviewRequests",
                ],
                &gate.worktree,
            )
            .map_err(unavailable)?;
        let value: Value = serde_json::from_str(&text).map_err(unavailable)?;
        let requests = value
            .get("reviewRequests")
            .and_then(Value::as_array)
            .ok_or_else(|| unavailable("GitHub response has no reviewRequests array"))?;
        Ok(requests
            .iter()
            .filter_map(|request| {
                request
                    .get("login")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        let slug = request.get("slug")?.as_str()?;
                        // gh exports Team.slug as the qualified org/team name.
                        if slug.contains('/') {
                            Some(slug.to_owned())
                        } else {
                            let org = request.pointer("/organization/login")?.as_str()?;
                            Some(format!("{org}/{slug}"))
                        }
                    })
            })
            .collect())
    }
}

impl PrBackend for GhBackend {
    fn requested_reviewers(&self, gate: &Gate, number: u64) -> Result<Vec<String>, ShepherdError> {
        GhBackend::requested_reviewers(self, gate, number)
    }

    fn branch_protected(&self, worktree: &Path, branch: &str) -> Result<bool, ShepherdError> {
        let args = super::protection::protection_args(branch)?;
        let gate = self.read_gate(worktree)?;
        let result = gate
            .gh(
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                worktree,
            )
            .map_err(unavailable)?;
        match result.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(unavailable("invalid branch protection response")),
        }
    }
    fn default_branch(&self, worktree: &Path) -> Result<String, ShepherdError> {
        Ok(crate::trusted::load_snapshot(&self.state_dir(), worktree)?.default_branch)
    }
    fn author(&self, worktree: &Path) -> Result<String, ShepherdError> {
        let text = self
            .read_gate(worktree)?
            .gh(&["api", "user", "--jq", ".login"], worktree)
            .map_err(unavailable)?;
        let author = text.trim();
        if author.is_empty() {
            return Err(unavailable("GitHub returned an empty author"));
        }
        Ok(author.into())
    }
    fn comments(
        &self,
        worktree: &Path,
        number: u64,
    ) -> Result<Vec<super::PrComment>, ShepherdError> {
        let gate = self.read_gate(worktree)?;
        let mut comments = Vec::new();
        for (kind, review) in [("pulls", true), ("issues", false)] {
            let route = format!("repos/{{owner}}/{{repo}}/{kind}/{number}/comments");
            let text = gate
                .gh(&["api", &route, "--paginate", "--slurp"], worktree)
                .map_err(unavailable)?;
            comments.extend(parse_comments(&text, review)?);
        }
        let route = format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews");
        let text = gate
            .gh(&["api", &route, "--paginate", "--slurp"], worktree)
            .map_err(unavailable)?;
        comments.extend(parse_reviews(&text)?);
        Ok(comments)
    }

    fn failed_checks(
        &self,
        worktree: &Path,
        number: u64,
    ) -> Result<Vec<super::FailedCheck>, ShepherdError> {
        let gate = self.read_gate(worktree)?;
        let text = gate
            .gh(
                &[
                    "pr",
                    "view",
                    &number.to_string(),
                    "--json",
                    "statusCheckRollup",
                ],
                worktree,
            )
            .map_err(unavailable)?;
        parse_failed_checks(&text)
    }

    fn view(&self, worktree: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError> {
        let gate = Gate {
            policy: BranchPolicy::new(&[], None, None)?,
            ticket_branch: branch.into(),
            worktree: worktree.into(),
            state_dir: self.state_dir(),
            config_dir: crate::config::resolve_config_dir(None),
        };
        match gate.gh(&["pr", "view", branch, "--json", "number,url,state,mergedAt,baseRefName,headRefName,headRefOid,statusCheckRollup,reviewDecision,comments,reviews"], worktree) {
            Ok(text) => parse_view(&text).map(Some),
            Err(ShepherdError::Runtime(error)) if {
                let text = error.to_string().to_ascii_lowercase();
                text.contains("no pull requests found") || text.contains("no pull request found for branch")
            } => Ok(None),
            Err(error) => Err(unavailable(error)),
        }
    }

    fn push(&self, gate: &Gate) -> Result<(), ShepherdError> {
        gate.git_push(&["-u", "origin", &gate.ticket_branch])
            .map(|_| ())
    }

    fn push_expected(&self, gate: &Gate, expected: &str) -> Result<(), ShepherdError> {
        gate.git_push_expected(&["-u", "origin", &gate.ticket_branch], expected)
            .map(|_| ())
    }

    fn create(
        &self,
        gate: &Gate,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrStatus, ShepherdError> {
        let mut args = Self::create_args(base, title, "-");
        args.extend(["--head".into(), gate.ticket_branch.clone()]);
        gate.gh_with_input(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &gate.worktree,
            body.as_bytes(),
        )?;
        Self::with_state(&gate.state_dir)
            .view(&gate.worktree, &gate.ticket_branch)?
            .ok_or_else(|| unavailable("created pull request could not be read"))
    }

    fn add_reviewers(
        &self,
        gate: &Gate,
        number: u64,
        reviewers: &[String],
    ) -> Result<(), ShepherdError> {
        if reviewers.is_empty() {
            return Ok(());
        }
        let mut args = vec!["pr".into(), "edit".into(), number.to_string()];
        for reviewer in reviewers {
            args.extend(["--add-reviewer".into(), reviewer.clone()]);
        }
        gate.gh(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &gate.worktree,
        )
        .map(|_| ())
    }

    fn reply(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<(), ShepherdError> {
        self.submit_reply(gate, number, comment_id, body)
            .map(|_| ())
    }

    fn reply_id(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<Option<u64>, ShepherdError> {
        let output = self.submit_reply(gate, number, comment_id, body)?;
        Ok(serde_json::from_str::<Value>(&output)
            .ok()
            .and_then(|raw| raw.get("id")?.as_u64())
            .or_else(|| {
                output
                    .trim()
                    .rsplit_once("#issuecomment-")
                    .and_then(|(_, id)| id.parse().ok())
            }))
    }
}

impl GhBackend {
    fn submit_reply(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<String, ShepherdError> {
        if let Some(id) = comment_id {
            let repository = gate.github_repository()?;
            let path = format!("repos/{repository}/pulls/{number}/comments/{id}/replies");
            gate.gh_with_input(
                &["api", "-X", "POST", &path, "-F", "body=@-"],
                &gate.worktree,
                body.as_bytes(),
            )
        } else {
            gate.gh_with_input(
                &["pr", "comment", &number.to_string(), "--body-file", "-"],
                &gate.worktree,
                body.as_bytes(),
            )
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct View {
    number: u64,
    url: String,
    state: String,
    merged_at: Option<String>,
    base_ref_name: String,
    head_ref_name: String,
    #[serde(default)]
    head_ref_oid: Option<String>,
    #[serde(default)]
    status_check_rollup: Option<Vec<Value>>,
    #[serde(default)]
    review_decision: Option<String>,
    #[serde(default)]
    comments: Vec<Value>,
}

pub fn parse_view(text: &str) -> Result<PrStatus, ShepherdError> {
    let raw: View = serde_json::from_str(text).map_err(unavailable)?;
    let state = match raw.state.as_str() {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        other => return Err(unavailable(format!("unknown PR state {other}"))),
    };
    let review = match raw.review_decision.as_deref().unwrap_or("") {
        "APPROVED" => ReviewDecision::Approved,
        "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
        "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired,
        _ => ReviewDecision::None,
    };
    Ok(PrStatus {
        number: raw.number,
        url: raw.url,
        state,
        merged_at: raw.merged_at,
        base: raw.base_ref_name,
        head: raw.head_ref_name,
        head_oid: raw.head_ref_oid,
        checks: map_checks(&Value::Array(raw.status_check_rollup.unwrap_or_default())),
        review,
        comment_ids: raw
            .comments
            .iter()
            .filter_map(|comment| {
                comment
                    .get("databaseId")
                    .and_then(Value::as_u64)
                    .or_else(|| comment.get("id").and_then(Value::as_u64))
                    .or_else(|| {
                        // gh exports opaque GraphQL IDs; its URL retains the
                        // numeric REST comment ID required by reply endpoints.
                        let (_, fragment) = comment.get("url")?.as_str()?.rsplit_once('#')?;
                        let digits = fragment
                            .strip_prefix("issuecomment-")
                            .or_else(|| fragment.strip_prefix("discussion_r"))?;
                        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                            return None;
                        }
                        digits.parse().ok()
                    })
            })
            .collect(),
    })
}

pub fn map_checks(rollup: &Value) -> Checks {
    let Some(checks) = rollup.as_array().filter(|checks| !checks.is_empty()) else {
        return Checks::None;
    };
    let mut pending = false;
    for check in checks {
        let conclusion = check
            .get("conclusion")
            .and_then(Value::as_str)
            .or_else(|| check.get("state").and_then(Value::as_str))
            .unwrap_or("");
        if matches!(
            conclusion,
            "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
        ) {
            return Checks::Failing;
        }
        let completed = check.get("status").and_then(Value::as_str).map_or(
            matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED"),
            |status| status == "COMPLETED",
        );
        if !completed || !matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED") {
            pending = true;
        }
    }
    if pending {
        Checks::Pending
    } else {
        Checks::Passing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Checks, PrState, ReviewDecision};
    use serde_json::json;

    #[test]
    fn parse_view_fixture() {
        let status = parse_view(include_str!("../../tests/fixtures/gh_pr_view.json")).unwrap();
        assert_eq!(status.state, PrState::Open);
        assert_eq!(status.checks, Checks::Failing);
        assert_eq!(status.review, ReviewDecision::ChangesRequested);
        assert_eq!(status.comment_ids, [123]);
    }

    #[test]
    fn checks_mapping() {
        for (rollup, expected) in [
            (json!([]), Checks::None),
            (
                json!([{"status":"COMPLETED","conclusion":"SUCCESS"}]),
                Checks::Passing,
            ),
            (
                json!([{"status":"COMPLETED","conclusion":"NEUTRAL"},{"status":"COMPLETED","conclusion":"SKIPPED"}]),
                Checks::Passing,
            ),
            (
                json!([{"status":"IN_PROGRESS","conclusion":null}]),
                Checks::Pending,
            ),
            (json!([{"status":"QUEUED"}]), Checks::Pending),
            (json!([{"state":"SUCCESS"}]), Checks::Passing),
            (json!([{"state":"PENDING"}]), Checks::Pending),
            (json!([{"state":"FAILURE"}]), Checks::Failing),
            (json!([{"state":"ERROR"}]), Checks::Failing),
        ] {
            assert_eq!(map_checks(&rollup), expected, "{rollup}");
        }
        for conclusion in [
            "FAILURE",
            "ERROR",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
        ] {
            assert_eq!(
                map_checks(&json!([
                    {"status":"IN_PROGRESS"},
                    {"status":"COMPLETED","conclusion":conclusion}
                ])),
                Checks::Failing
            );
        }
    }

    #[test]
    fn create_never_draft() {
        let args = GhBackend::create_args("main", "title", "-");
        assert!(!args.iter().any(|arg| arg == "--draft"));
        assert_eq!(
            args,
            [
                "pr",
                "create",
                "--base",
                "main",
                "--title",
                "title",
                "--body-file",
                "-"
            ]
        );
    }

    #[test]
    fn graphql_comment_ids_use_numeric_url_fragments() {
        let mut raw: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/gh_pr_view.json")).unwrap();
        raw["comments"] = json!([
            {"id":"IC_opaque", "url":"https://github.com/example/project/pull/42#issuecomment-234"},
            {"id":"PRRC_opaque", "url":"https://github.com/example/project/pull/42#discussion_r345"},
            {"databaseId":456,"id":"IC_opaque"},
            {"id":567},
            {"id":"IC_unusable", "url":"https://github.com/example/project/pull/42#issuecomment-+678"},
            {"id":"IC_unusable", "url":"https://github.com/example/project/pull/42#other-789"}
        ]);
        // Review IDs are a different object kind and must not become comment IDs.
        raw["reviews"] = json!([{"id":"PRR_opaque"}]);
        assert_eq!(
            parse_view(&raw.to_string()).unwrap().comment_ids,
            [234, 345, 456, 567]
        );
    }

    #[test]
    fn malformed_status_is_unavailable() {
        assert!(matches!(
            parse_view("{}"),
            Err(ShepherdError::GithubUnavailable(_))
        ));
    }
}

pub fn parse_comments(text: &str, review: bool) -> Result<Vec<super::PrComment>, ShepherdError> {
    let pages: Vec<Vec<Value>> = serde_json::from_str(text).map_err(unavailable)?;
    pages
        .into_iter()
        .flatten()
        .map(|raw| {
            Ok(super::PrComment {
                kind: Some(if review {
                    super::CommentKind::Review
                } else {
                    super::CommentKind::Issue
                }),
                id: raw
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| unavailable("comment has no numeric id"))?,
                author: if raw.get("user") == Some(&Value::Null) {
                    "ghost".into()
                } else {
                    raw.pointer("/user/login")
                        .and_then(Value::as_str)
                        .ok_or_else(|| unavailable("comment has no author"))?
                        .into()
                },
                path: raw.get("path").and_then(Value::as_str).map(str::to_owned),
                line: raw.get("line").and_then(Value::as_u64),
                body: match raw.get("body") {
                    Some(Value::Null) => String::new(),
                    Some(Value::String(body)) => body.clone(),
                    _ => return Err(unavailable("comment has no body")),
                },
                review,
            })
        })
        .collect()
}

pub fn parse_reviews(text: &str) -> Result<Vec<super::PrComment>, ShepherdError> {
    let pages: Vec<Vec<Value>> = serde_json::from_str(text).map_err(unavailable)?;
    let bodies: Vec<_> = pages
        .into_iter()
        .flatten()
        .filter(|review| {
            review.get("state").and_then(Value::as_str) == Some("CHANGES_REQUESTED")
                && review
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| !body.trim().is_empty())
        })
        .collect();
    let mut comments = parse_comments(
        &serde_json::to_string(&vec![bodies]).map_err(unavailable)?,
        false,
    )?;
    for comment in &mut comments {
        comment.kind = Some(super::CommentKind::ReviewBody);
    }
    Ok(comments)
}

pub fn parse_failed_checks(text: &str) -> Result<Vec<super::FailedCheck>, ShepherdError> {
    let raw: Value = serde_json::from_str(text).map_err(unavailable)?;
    let checks = raw
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .ok_or_else(|| unavailable("missing statusCheckRollup"))?;
    Ok(checks
        .iter()
        .filter(|check| {
            matches!(
                check
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .or_else(|| check.get("state").and_then(Value::as_str)),
                Some("FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED")
            )
        })
        .map(|check| super::FailedCheck {
            name: check
                .get("name")
                .or_else(|| check.get("context"))
                .and_then(Value::as_str)
                .unwrap_or("CI check")
                .into(),
            url: check
                .get("detailsUrl")
                .or_else(|| check.get("targetUrl"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
        })
        .collect())
}

#[cfg(test)]
mod comment_tests {
    use super::*;

    #[test]
    fn nullable_comment_fields_keep_the_entire_page() {
        for review in [true, false] {
            let comments = parse_comments(
                include_str!("../../tests/fixtures/gh_comments_nullable.json"),
                review,
            )
            .unwrap();
            assert_eq!(comments.len(), 4);
            assert_eq!(comments[0].author, "ghost");
            assert_eq!(comments[0].body, "Please check the empty input");
            assert_eq!(comments[1].author, "reviewer");
            assert_eq!(comments[1].body, "");
            assert_eq!(comments[2].author, "ghost");
            assert_eq!(comments[2].body, "");
            assert_eq!(comments[3].id, 24);
            assert_eq!(comments[3].body, "This comment stays available");
        }
        let reviews = parse_reviews(
            r#"[[{"id":25,"state":"CHANGES_REQUESTED","user":null,"body":"Check the guard"}]]"#,
        )
        .unwrap();
        assert_eq!(reviews[0].author, "ghost");
    }

    #[test]
    fn paginated_review_and_issue_comments_keep_context() {
        let comments = parse_comments(r#"[[{"id":11,"user":{"login":"reviewer"},"path":"src/lib.rs","line":7,"body":"Check the edge"}],[{"id":12,"user":{"login":"another"},"path":"src/main.rs","line":null,"body":"Old line"}]]"#, true).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, 11);
        assert_eq!(comments[0].author, "reviewer");
        assert_eq!(comments[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(comments[0].line, Some(7));
        assert!(comments[0].review);
        assert_eq!(comments[1].line, None);
        let issue = parse_comments(
            r#"[[{"id":11,"user":{"login":"reviewer"},"body":"Question"}]]"#,
            false,
        )
        .unwrap();
        assert!(!issue[0].review);
        assert_eq!(issue[0].path, None);
    }

    #[test]
    fn changes_requested_review_bodies_are_comments() {
        let comments = parse_reviews(r#"[[{"id":11,"state":"CHANGES_REQUESTED","user":{"login":"reviewer"},"body":"Fix the missing guard"},{"id":12,"state":"APPROVED","user":{"login":"another"},"body":"Looks good"},{"id":13,"state":"CHANGES_REQUESTED","user":{"login":"reviewer"},"body":" "}]]"#).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, 11);
        assert_eq!(comments[0].body, "Fix the missing guard");
    }

    #[test]
    fn failing_checks_keep_names_and_urls() {
        let checks = parse_failed_checks(r#"{"statusCheckRollup":[{"name":"unit","conclusion":"FAILURE","detailsUrl":"https://example.invalid/unit"},{"context":"lint","state":"ERROR","targetUrl":"https://example.invalid/lint"},{"name":"build","conclusion":"SUCCESS"}]}"#).unwrap();
        assert_eq!(
            checks
                .iter()
                .map(|check| (check.name.as_str(), check.url.as_str()))
                .collect::<Vec<_>>(),
            [
                ("unit", "https://example.invalid/unit"),
                ("lint", "https://example.invalid/lint")
            ]
        );
    }

    #[test]
    fn malformed_comment_response_errors() {
        for text in ["{}", "[[{}]]", r#"[[{"id":1,"body":"Question"}]]"#] {
            assert_eq!(
                parse_comments(text, true).unwrap_err().code(),
                "github_unavailable"
            );
        }
    }
}
