use crate::{error::ShepherdError, gate::BranchPolicy};

#[derive(Debug)]
pub struct PushTarget {
    pub remote: String,
    pub refspecs: Vec<String>,
    pub flags: Vec<String>,
}

pub fn parse_push_args(args: &[&str]) -> PushTarget {
    let mut result = PushTarget {
        remote: String::new(),
        refspecs: Vec::new(),
        flags: Vec::new(),
    };
    for arg in args {
        if arg.starts_with('-') {
            result.flags.push((*arg).into());
        } else if result.remote.is_empty() {
            result.remote = (*arg).into();
        } else {
            result.refspecs.push((*arg).into());
        }
    }
    result
}

pub fn check_push(
    t: &PushTarget,
    ticket_branch: &str,
    policy: &BranchPolicy,
) -> Result<(), ShepherdError> {
    let deny = |reason: &str| Err(ShepherdError::PolicyDenied(reason.into()));
    if t.remote.is_empty() || t.refspecs.is_empty() || ticket_branch.is_empty() {
        return deny("push requires an explicit remote and ticket refspec");
    }
    if t.flags.iter().any(|flag| {
        !matches!(
            flag.as_str(),
            "-u" | "--set-upstream" | "--porcelain" | "--quiet" | "-q"
        )
    }) {
        return deny("unsupported or unsafe push flag");
    }
    if policy.is_protected(ticket_branch) {
        return deny("ticket branch is protected");
    }
    for refspec in &t.refspecs {
        if refspec.starts_with('+') || refspec.contains('*') {
            return deny("forced or wildcard refspec");
        }
        let (source, destination) = refspec.split_once(':').unwrap_or((refspec, refspec));
        if source.is_empty() || destination.is_empty() || destination.contains(':') {
            return deny("delete or malformed refspec");
        }
        let destination = destination
            .strip_prefix("refs/heads/")
            .unwrap_or(destination);
        if destination != ticket_branch || policy.is_protected(destination) {
            return deny("push destination must be the unprotected ticket branch");
        }
    }
    Ok(())
}

fn denied(reason: &str) -> ShepherdError {
    ShepherdError::PolicyDenied(reason.into())
}

fn check_pr(args: &[&str]) -> Result<(), ShepherdError> {
    let Some(verb) = args.first() else {
        return Err(denied("missing PR command"));
    };
    if *verb == "merge" {
        return Err(denied("merging PRs is forbidden"));
    }
    // Flags before the verb and unknown commands fail closed, so a command
    // cannot be hidden behind persistent -R/--repo flags or aliases.
    if !matches!(
        *verb,
        "view"
            | "list"
            | "status"
            | "checks"
            | "diff"
            | "create"
            | "edit"
            | "comment"
            | "review"
            | "ready"
            | "close"
            | "reopen"
            | "checkout"
            | "lock"
            | "unlock"
    ) {
        return Err(denied("unsupported PR command or command-prefix option"));
    }
    let review = *verb == "review";
    let mut explicit_review = false;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i];
        if let Some(long) = arg.strip_prefix("--") {
            let (flag, inline) = long
                .split_once('=')
                .map_or((long, false), |(flag, _)| (flag, true));
            if flag.contains("auto-merge") || flag == "auto" {
                return Err(denied("automatic merge is forbidden"));
            }
            if review && flag == "approve" {
                return Err(denied("approving PRs is forbidden"));
            }
            if review && matches!(flag, "comment" | "request-changes") {
                explicit_review |= !inline
                    || long.split_once('=').is_some_and(|(_, value)| {
                        matches!(value, "true" | "TRUE" | "True" | "1" | "t" | "T")
                    });
            }
            if !inline
                && matches!(
                    flag,
                    "repo"
                        | "body"
                        | "body-file"
                        | "title"
                        | "base"
                        | "head"
                        | "json"
                        | "jq"
                        | "template"
                        | "label"
                        | "assignee"
                        | "reviewer"
                        | "add-reviewer"
                        | "remove-reviewer"
                        | "add-label"
                        | "remove-label"
                        | "add-assignee"
                        | "remove-assignee"
                        | "milestone"
                        | "state"
                        | "search"
                        | "limit"
                        | "author"
                )
            {
                i += 1;
                if i >= args.len() {
                    return Err(denied("missing PR option value"));
                }
            }
        } else if arg.starts_with('-') && arg != "-" {
            let mut chars = arg[1..].chars().peekable();
            while let Some(flag) = chars.next() {
                if review && flag == 'a' {
                    return Err(denied("approving PRs is forbidden"));
                }
                if review && matches!(flag, 'c' | 'r') {
                    explicit_review = true;
                }
                let takes_value = matches!(
                    flag,
                    'R' | 'b' | 'F' | 't' | 'B' | 'H' | 'q' | 'l' | 'm' | 'L' | 's'
                ) || (!review && matches!(flag, 'a' | 'r'));
                if takes_value {
                    if chars.peek().is_none() {
                        i += 1;
                        if i >= args.len() {
                            return Err(denied("missing PR option value"));
                        }
                    }
                    break;
                }
                if review && !matches!(flag, 'c' | 'r' | 'h') {
                    return Err(denied("unsupported review option"));
                }
            }
        }
        i += 1;
    }
    // An interactive review can select APPROVE after the gate has run.
    if review && !explicit_review {
        return Err(denied("review requires an explicit non-approval action"));
    }
    Ok(())
}

fn api_path(endpoint: &str) -> Result<String, ShepherdError> {
    let endpoint = endpoint.split('?').next().unwrap_or(endpoint);
    if endpoint.is_empty()
        || endpoint
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || endpoint.contains(['%', '\\', '#'])
    {
        return Err(denied("encoded or ambiguous API endpoint is forbidden"));
    }
    let path = if let Some((scheme, authority)) = endpoint.split_once("://") {
        if !matches!(scheme, "http" | "https") {
            return Err(denied("unsupported API endpoint scheme"));
        }
        authority.split_once('/').map_or("", |(_, path)| path)
    } else {
        endpoint
    };
    if path.split('/').any(|segment| matches!(segment, "." | "..")) {
        return Err(denied("ambiguous API path is forbidden"));
    }
    Ok(path.trim_matches('/').to_ascii_lowercase())
}

fn check_api(args: &[&str]) -> Result<(), ShepherdError> {
    let mut endpoint = None;
    let mut method = "GET";
    let mut payload = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg.starts_with('-') {
            if !arg.is_ascii() {
                return Err(denied("unsupported API option"));
            }
            let (flag, inline): (&str, Option<&str>) = if arg.starts_with("--") {
                arg.split_once('=')
                    .map_or((arg, None), |(flag, value)| (flag, Some(value)))
            } else if arg.len() > 2 {
                (
                    &arg[..2],
                    Some(arg[2..].strip_prefix('=').unwrap_or(&arg[2..])),
                )
            } else {
                (arg, None)
            };
            let takes_value = matches!(
                flag,
                "-X" | "--method"
                    | "-f"
                    | "--raw-field"
                    | "-F"
                    | "--field"
                    | "--input"
                    | "-H"
                    | "--header"
                    | "-q"
                    | "--jq"
                    | "-t"
                    | "--template"
                    | "--hostname"
                    | "--cache"
                    | "-p"
                    | "--preview"
            );
            if takes_value {
                let value = if let Some(value) = inline {
                    value
                } else {
                    i += 1;
                    args.get(i)
                        .copied()
                        .ok_or_else(|| denied("missing API option value"))?
                };
                if matches!(flag, "-X" | "--method") {
                    method = value;
                }
                if matches!(flag, "-f" | "--raw-field" | "-F" | "--field" | "--input") {
                    payload = true;
                }
            } else if inline.is_some()
                || !matches!(
                    flag,
                    "--paginate" | "--slurp" | "--silent" | "--include" | "-i" | "--verbose"
                )
            {
                return Err(denied("unsupported API option"));
            }
        } else if endpoint.replace(arg).is_some() {
            return Err(denied("multiple API endpoints are forbidden"));
        }
        i += 1;
    }
    let path = api_path(endpoint.ok_or_else(|| denied("missing API endpoint"))?)?;
    let write = payload || !method.eq_ignore_ascii_case("GET");
    if path == "graphql"
        || path.ends_with("/graphql")
        || path.contains("/merges")
        || (path.ends_with("/merge") && write)
    {
        return Err(denied("merge API is forbidden"));
    }
    let sensitive = path.contains("git/refs")
        || path.contains("actions/workflows/")
        || path.split('/').any(|segment| {
            matches!(
                segment,
                "contents" | "branches" | "merge-upstream" | "merges" | "protection"
            )
        });
    if write && sensitive {
        return Err(denied("repository or workflow mutations are forbidden"));
    }
    if write && (path.ends_with("/reviews") || path.contains("/reviews/")) {
        return Err(denied("review API mutations are forbidden"));
    }
    if write {
        let parts: Vec<_> = path.split('/').collect();
        let comment_route = match parts.as_slice() {
            ["repos", _, _, "issues" | "pulls", number, "comments"] => {
                number.parse::<u64>().is_ok()
            }
            [
                "repos",
                _,
                _,
                "pulls",
                number,
                "comments",
                comment,
                "replies",
            ] => number.parse::<u64>().is_ok() && comment.parse::<u64>().is_ok(),
            _ => false,
        };
        if !comment_route || (!payload && !method.eq_ignore_ascii_case("POST")) {
            return Err(denied("API writes are restricted to comment creation"));
        }
        if !matches!(method.to_ascii_uppercase().as_str(), "GET" | "POST") {
            return Err(denied("only comment creation is permitted"));
        }
    }
    Ok(())
}

pub fn check_gh(args: &[&str]) -> Result<(), ShepherdError> {
    let Some((command, rest)) = args.split_first() else {
        return Err(denied("missing gh command"));
    };
    match *command {
        "pr" => check_pr(rest),
        "api" => check_api(rest),
        "repo"
            if rest
                .first()
                .is_some_and(|verb| matches!(*verb, "view" | "list" | "clone")) =>
        {
            Ok(())
        }
        "workflow"
            if rest
                .first()
                .is_some_and(|verb| matches!(*verb, "view" | "list")) =>
        {
            Ok(())
        }
        "issue" | "search" | "run" | "version" => Ok(()),
        _ => Err(denied("unsupported gh command or global option")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::ShepherdError, gate::BranchPolicy};

    #[test]
    fn check_push_table() {
        let policy = BranchPolicy::new(&[], None, None).unwrap();
        for args in [
            vec!["origin", "HEAD:main"],
            vec!["origin", "+feat/x:feat/x"],
            vec!["origin", "refs/heads/main"],
            vec!["origin", "feat/x:refs/heads/main"],
            vec!["origin", "--all"],
            vec!["origin", "--mirror"],
            vec!["origin", "--tags", "feat/x"],
            vec!["origin", "--force", "feat/x"],
            vec!["origin", "--force-with-lease=feat/x", "feat/x"],
            vec!["origin", "-f", "feat/x"],
            vec!["origin", "--no-verify", "feat/x"],
            vec!["origin", ":feat/x"],
            vec!["origin", "--delete", "feat/x"],
            vec!["origin", "feat/y"],
            vec!["origin"],
            vec!["origin", "--unknown", "feat/x"],
        ] {
            assert!(
                matches!(
                    check_push(&parse_push_args(&args), "feat/x", &policy),
                    Err(ShepherdError::PolicyDenied(_))
                ),
                "{args:?}"
            );
        }
        for args in [
            vec!["origin", "feat/x"],
            vec!["origin", "HEAD:feat/x"],
            vec!["-u", "origin", "feat/x"],
            vec!["origin", "HEAD:refs/heads/feat/x"],
        ] {
            check_push(&parse_push_args(&args), "feat/x", &policy).unwrap();
        }
    }

    #[test]
    fn check_gh_table() {
        for args in [
            vec!["pr", "merge", "12"],
            vec!["pr", "merge", "--auto", "12"],
            vec!["pr", "review", "12", "--approve"],
            vec!["api", "-X", "PUT", "repos/o/r/pulls/12/merge"],
            vec!["api", "--method=PUT", "repos/o/r/pulls/12/merge"],
            vec!["api", "repos/o/r/merges", "-f", "base=main"],
            vec!["pr", "edit", "12", "--enable-auto-merge"],
            vec!["pr", "review", "12", "-a"],
            vec![
                "api",
                "repos/o/r/pulls/12/reviews",
                "--input",
                "fixture.json",
            ],
        ] {
            assert!(
                matches!(check_gh(&args), Err(ShepherdError::PolicyDenied(_))),
                "{args:?}"
            );
        }
        for args in [
            vec!["pr", "view", "12", "--json", "state"],
            vec!["pr", "create", "--title", "t", "--body", "b"],
            vec!["pr", "create", "--body", "Explain auto-merge policy"],
            vec![
                "api",
                "repos/o/r/pulls/12/comments/9/replies",
                "-f",
                "body=x",
            ],
        ] {
            check_gh(&args).unwrap();
        }
    }
    #[test]
    fn api_policy_closes_encoded_and_method_bypasses() {
        for args in [
            vec![
                "api",
                "/graphql",
                "-f",
                "query=mutation { mergePullRequest }",
            ],
            vec![
                "api",
                "https://api.github.com/graphql",
                "-f",
                "query=mutation { addPullRequestReview }",
            ],
            vec!["api", "-X=PUT", "repos/o/r/pulls/12/merge"],
            vec!["api", "-XPUT", "repos/o/r/pulls/12/merge"],
            vec!["api", "-X", "PUT", "repos/o/r/pulls/12/%6derge"],
            vec!["api", "-X", "PUT", "repos/o/r/pulls/12/merge/..%2fmerge"],
            vec![
                "api",
                "repos/o/r/pulls/12/reviews/4/events",
                "--input=ordinary.json",
            ],
            vec![
                "api",
                "-XPOST",
                "repos/o/r/pulls/12/reviews",
                "-Fevent=@ordinary.txt",
            ],
            vec!["pr", "-R", "o/r", "merge", "12"],
            vec!["pr", "--repo=o/r", "review", "12", "--approve"],
            vec!["pr", "review", "12", "-ab", "ordinary review"],
            vec!["pr", "review", "12", "-ca"],
            vec!["pr", "review", "12", "--comment=false"],
            vec!["pr", "review", "12"],
            vec!["api", "-é", "user"],
        ] {
            assert!(
                matches!(check_gh(&args), Err(ShepherdError::PolicyDenied(_))),
                "{args:?}"
            );
        }
    }

    #[test]
    fn api_policy_ignores_literal_body_and_option_values() {
        for args in [
            vec![
                "api",
                "-X",
                "POST",
                "repos/o/r/pulls/12/comments/9/replies",
                "-f",
                "body=Document /merges and /graphql routes",
            ],
            vec![
                "api",
                "repos/o/r/pulls/12/comments/9/replies",
                "-fbody=Use --enable-auto-merge only manually",
            ],
            vec![
                "pr",
                "create",
                "--title",
                "--enable-auto-merge",
                "--body",
                "Explain /merges",
            ],
            vec!["pr", "comment", "12", "--body", "--auto"],
            vec![
                "pr",
                "review",
                "12",
                "--comment",
                "--body",
                "An ordinary review mentions --approve",
            ],
            vec!["api", "repos/o/r/pulls/12/merge", "-XGET"],
        ] {
            check_gh(&args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
        }
    }
}
