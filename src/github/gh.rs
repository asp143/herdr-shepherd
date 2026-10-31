use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::{Checks, PrBackend, PrState, PrStatus, ReviewDecision};
use crate::{
    error::ShepherdError,
    gate::{BranchPolicy, Gate},
};

pub struct GhBackend;

fn unavailable(error: impl std::fmt::Display) -> ShepherdError {
    ShepherdError::GithubUnavailable(error.to_string())
}

impl GhBackend {
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
    fn view(&self, worktree: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError> {
        let gate = Gate {
            policy: BranchPolicy::new(&[], None, None)?,
            ticket_branch: branch.into(),
            worktree: worktree.into(),
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
        self.view(&gate.worktree, &gate.ticket_branch)?
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
        if let Some(id) = comment_id {
            let repository = gate.gh(
                &[
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "-q",
                    ".nameWithOwner",
                ],
                &gate.worktree,
            )?;
            let repository = repository.trim();
            let parts: Vec<_> = repository.split('/').collect();
            if parts.len() != 2
                || parts.iter().any(|part| {
                    part.is_empty()
                        || *part == "."
                        || *part == ".."
                        || !part
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                })
            {
                return Err(unavailable("GitHub returned an invalid nameWithOwner"));
            }
            let path = format!("repos/{repository}/pulls/{number}/comments/{id}/replies");
            gate.gh(
                &["api", "-X", "POST", &path, "-f", &format!("body={body}")],
                &gate.worktree,
            )
            .map(|_| ())
        } else {
            gate.gh_with_input(
                &["pr", "comment", &number.to_string(), "--body-file", "-"],
                &gate.worktree,
                body.as_bytes(),
            )
            .map(|_| ())
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
