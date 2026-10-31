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

pub fn check_gh(args: &[&str]) -> Result<(), ShepherdError> {
    let deny = |reason: &str| Err(ShepherdError::PolicyDenied(reason.into()));
    // Do not dispatch arbitrary gh aliases: an alias may hide a forbidden command.
    let Some(command) = args.first() else {
        return deny("missing gh command");
    };
    if !matches!(
        *command,
        "pr" | "api" | "repo" | "issue" | "search" | "run" | "workflow" | "auth" | "version"
    ) {
        return deny("unsupported gh command or global option");
    }
    if args
        .iter()
        .any(|arg| arg.contains("auto-merge") || *arg == "--auto")
    {
        return deny("automatic merge is forbidden");
    }
    if *command == "pr" {
        if args.get(1) == Some(&"merge") {
            return deny("merging PRs is forbidden");
        }
        if args.get(1) == Some(&"review")
            && args
                .iter()
                .any(|arg| *arg == "-a" || arg.starts_with("--approve"))
        {
            return deny("approving PRs is forbidden");
        }
    }
    if *command == "api" {
        let mut method = "GET";
        for (index, arg) in args.iter().enumerate() {
            if matches!(*arg, "-X" | "--method") {
                method = args.get(index + 1).copied().unwrap_or("");
            } else if let Some(value) = arg
                .strip_prefix("--method=")
                .or_else(|| arg.strip_prefix("-X").filter(|value| !value.is_empty()))
            {
                method = value;
            }
        }
        for arg in &args[1..] {
            let path = arg.split('?').next().unwrap_or(arg).trim_end_matches('/');
            if path == "graphql"
                || path.contains("/merges")
                || (path.ends_with("/merge") && method.eq_ignore_ascii_case("PUT"))
            {
                return deny("merge API is forbidden");
            }
            // The review API can approve without going through `gh pr review`.
            if arg.eq_ignore_ascii_case("event=APPROVE") || arg.contains("\"event\":\"APPROVE\"") {
                return deny("approving PRs is forbidden");
            }
        }
    }
    Ok(())
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
        ] {
            assert!(
                matches!(check_gh(&args), Err(ShepherdError::PolicyDenied(_))),
                "{args:?}"
            );
        }
        for args in [
            vec!["pr", "view", "12", "--json", "state"],
            vec!["pr", "create", "--title", "t", "--body", "b"],
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
}
