//! Read-only branch protection warnings, cached in the project's durable state.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{error::ShepherdError, github::PrBackend, store::Store};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProtectionWarning {
    pub branch: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProtectionCache {
    #[serde(default)]
    pub repository: Option<String>,
    pub default_branch: Option<String>,
    pub default_checked_at: Option<u64>,
    pub default_error: Option<String>,
    pub branches: Vec<ProtectionCacheEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProtectionCacheEntry {
    pub branch: String,
    pub checked_at: u64,
    pub warning: Option<ProtectionWarning>,
}

pub fn check_protection(
    gh: &dyn PrBackend,
    worktree: &Path,
    branches: &[String],
) -> Vec<ProtectionWarning> {
    let mut checked = std::collections::HashSet::new();
    branches
        .iter()
        .filter(|branch| checked.insert(branch.as_str()))
        .filter_map(|branch| {
            let reason = match gh.branch_protected(worktree, branch) {
                Ok(true) => return None,
                Ok(false) => "unprotected".into(),
                Err(error) => format!("unknown: {error}"),
            };
            Some(ProtectionWarning {
                branch: branch.clone(),
                reason,
            })
        })
        .collect()
}

fn fresh(checked_at: u64, now: u64) -> bool {
    now.checked_sub(checked_at)
        .is_some_and(|age| age < 24 * 60 * 60)
}

/// Cache successful reads; retry unknown results. A new base is always read.
pub fn cached_protection(
    store: &Store,
    gh: &dyn PrBackend,
    base: &str,
    now: u64,
) -> Result<Vec<ProtectionWarning>, ShepherdError> {
    let data = store.read()?;
    let mut refreshed = data.protection_cache.clone();
    let cache = &mut refreshed;
    let repository = data.trust.as_ref().map(|trust| trust.origin_url.clone());
    if cache.repository != repository {
        *cache = ProtectionCache {
            repository,
            ..Default::default()
        };
    }
    if cache.default_error.is_some() || cache.default_checked_at.is_none_or(|at| !fresh(at, now)) {
        cache.default_checked_at = Some(now);
        match data
            .trust
            .as_ref()
            .map(|trust| Ok(trust.default_branch.clone()))
            .unwrap_or_else(|| gh.default_branch(&data.project_root))
        {
            Ok(branch) if !branch.is_empty() => {
                cache.default_branch = Some(branch);
                cache.default_error = None;
            }
            result => {
                cache.default_branch = None;
                cache.default_error = Some(match result {
                    Err(error) => error.to_string(),
                    Ok(_) => "default branch is empty".into(),
                });
            }
        }
    }
    let mut branches = vec![base.to_owned()];
    if let Some(branch) = &cache.default_branch
        && branch != base
    {
        branches.push(branch.clone());
    }
    let mut warnings = Vec::new();
    for branch in branches {
        let current = cache.branches.iter().find(|entry| {
            entry.branch == branch
                && fresh(entry.checked_at, now)
                && !entry
                    .warning
                    .as_ref()
                    .is_some_and(|warning| warning.reason.starts_with("unknown:"))
        });
        let warning = if let Some(entry) = current {
            entry.warning.clone()
        } else {
            let warning = check_protection(gh, &data.project_root, std::slice::from_ref(&branch))
                .into_iter()
                .next();
            cache.branches.retain(|entry| entry.branch != branch);
            if warning
                .as_ref()
                .is_none_or(|warning| !warning.reason.starts_with("unknown:"))
            {
                cache.branches.push(ProtectionCacheEntry {
                    branch,
                    checked_at: now,
                    warning: warning.clone(),
                });
            }
            warning
        };
        warnings.extend(warning);
    }
    if let Some(error) = &cache.default_error {
        warnings.push(ProtectionWarning {
            branch: "(default branch)".into(),
            reason: format!("unknown: {error}"),
        });
    }
    store.update(|current| {
        current.protection_cache = refreshed;
        Ok(())
    })?;
    Ok(warnings)
}

pub fn warning_line(repo: &Path, warning: &ProtectionWarning) -> String {
    if warning.reason == "unprotected" {
        format!(
            "{}: {} has no branch protection; shepherd's push guard is best effort without it",
            repo.display(),
            warning.branch
        )
    } else {
        format!(
            "{}: {} branch protection {}",
            repo.display(),
            warning.branch,
            warning.reason
        )
    }
}

/// Persist warnings on the ticket so status and the integrated lead summary can show them.
pub fn check_started_ticket(
    store: &Store,
    gh: &dyn PrBackend,
    h: &dyn crate::herdr::Herdr,
    id: &str,
    now: u64,
    notifications: bool,
) -> Result<(), ShepherdError> {
    let data = store.read()?;
    let ticket = data
        .tickets
        .iter()
        .find(|ticket| ticket.id == id)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
    let warnings = cached_protection(store, gh, &ticket.base, now)?;
    store.update(|data| {
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        ticket.protection_warnings = warnings.clone();
        Ok(())
    })?;
    let notifier = crate::notify::Notifier::new(h, notifications);
    for warning in &warnings {
        notifier.send(&warning_line(&data.project_root, warning), "");
    }
    Ok(())
}

/// Refresh status through the same cache used by ticket starts.
pub fn refresh_status(
    store: &Store,
    gh: &dyn PrBackend,
    now: u64,
) -> Result<crate::store::StoreData, ShepherdError> {
    let tickets = store.read()?.tickets;
    for ticket in tickets {
        let warnings = cached_protection(store, gh, &ticket.base, now)?;
        store.update(|data| {
            if let Some(current) = data
                .tickets
                .iter_mut()
                .find(|current| current.id == ticket.id)
            {
                current.protection_warnings = warnings;
            }
            Ok(())
        })?;
    }
    store.read()
}

pub fn status_warnings(data: &crate::store::StoreData) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut branches = std::collections::BTreeSet::new();
    for ticket in &data.tickets {
        branches.insert(ticket.base.as_str());
        branches.extend(
            ticket
                .protection_warnings
                .iter()
                .map(|warning| warning.branch.as_str()),
        );
    }
    if let Some(branch) = &data.protection_cache.default_branch {
        branches.insert(branch);
    }
    for branch in branches {
        let warning = if let Some(entry) = data
            .protection_cache
            .branches
            .iter()
            .find(|entry| entry.branch == branch)
        {
            entry.warning.as_ref()
        } else if branch == "(default branch)" && data.protection_cache.default_checked_at.is_some()
        {
            None
        } else {
            data.tickets
                .iter()
                .flat_map(|ticket| &ticket.protection_warnings)
                .find(|warning| warning.branch == branch)
        };
        if let Some(warning) = warning {
            warnings.push(warning_line(&data.project_root, warning));
        }
    }
    if let Some(error) = &data.protection_cache.default_error {
        warnings.push(warning_line(
            &data.project_root,
            &ProtectionWarning {
                branch: "(default branch)".into(),
                reason: format!("unknown: {error}"),
            },
        ));
    }
    warnings
}

/// Use gh's repository placeholders so branch reads cannot select another repo.
pub fn protection_args(branch: &str) -> Result<Vec<String>, ShepherdError> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || branch
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !b"/._-".contains(&byte))
    {
        return Err(ShepherdError::Usage("invalid protection branch".into()));
    }
    Ok(vec![
        "api".into(),
        "-X".into(),
        "GET".into(),
        format!("repos/{{owner}}/{{repo}}/branches/{branch}"),
        "--jq".into(),
        ".protected".into(),
    ])
}

fn read_gate(worktree: &Path) -> Result<crate::gate::Gate, ShepherdError> {
    Ok(crate::gate::Gate {
        policy: crate::gate::BranchPolicy::new(&[], None, None)?,
        ticket_branch: String::new(),
        worktree: worktree.into(),
        state_dir: crate::gate::state_dir(),
        config_dir: crate::config::resolve_config_dir(None),
    })
}

pub fn gh_branch_protected(worktree: &Path, branch: &str) -> Result<bool, ShepherdError> {
    let args = protection_args(branch)?;
    let text = read_gate(worktree)?
        .gh(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            worktree,
        )
        .map_err(|error| ShepherdError::GithubUnavailable(error.to_string()))?;
    match text.trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ShepherdError::GithubUnavailable(
            "invalid branch protection response".into(),
        )),
    }
}

pub fn gh_default_branch(worktree: &Path) -> Result<String, ShepherdError> {
    Ok(crate::trusted::load_snapshot(&crate::gate::state_dir(), worktree)?.default_branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::fake::FakeBackend;

    fn backend(protected: Result<bool, &'static str>) -> FakeBackend {
        let gh = FakeBackend::new(vec![]);
        *gh.default_branch.borrow_mut() = Ok("main".into());
        for branch in ["main", "develop", "release/topic"] {
            gh.protected
                .borrow_mut()
                .insert(branch.into(), protected.map_err(str::to_owned));
        }
        gh
    }
    fn reads(gh: &FakeBackend) -> usize {
        gh.calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with("branch_protected "))
            .count()
    }

    #[test]
    fn warns_on_unprotected_base() {
        let gh = backend(Ok(false));
        assert_eq!(
            check_protection(&gh, Path::new("."), &["develop".into()]),
            vec![ProtectionWarning {
                branch: "develop".into(),
                reason: "unprotected".into()
            }]
        );
    }
    #[test]
    fn no_warning_when_protected() {
        assert!(check_protection(&backend(Ok(true)), Path::new("."), &["main".into()]).is_empty());
    }
    #[test]
    fn error_reported_as_unknown_not_fatal() {
        let warnings = check_protection(&backend(Err("offline")), Path::new("."), &["main".into()]);
        assert_eq!(warnings[0].reason, "unknown: offline");
        // Existing adapters that do not support this read remain non-fatal.
        assert!(
            check_protection(&FakeBackend::new(vec![]), Path::new("."), &["main".into()])[0]
                .reason
                .starts_with("unknown:")
        );
    }
    #[test]
    fn unknown_results_retry_on_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let gh = backend(Err("offline"));
        assert_eq!(
            cached_protection(&store, &gh, "main", 100).unwrap()[0].reason,
            "unknown: offline"
        );
        gh.protected.borrow_mut().insert("main".into(), Ok(true));
        assert!(
            cached_protection(&store, &gh, "main", 101)
                .unwrap()
                .is_empty()
        );
        assert_eq!(reads(&gh), 2);
    }

    #[test]
    fn cached_for_24h() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let gh = backend(Ok(false));
        let warnings = cached_protection(&store, &gh, "develop", 100).unwrap();
        assert_eq!(warnings.len(), 2);
        assert_eq!(reads(&gh), 2);
        let reopened = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        assert_eq!(
            cached_protection(&reopened, &gh, "develop", 86_499).unwrap(),
            warnings
        );
        assert_eq!(reads(&gh), 2);
        cached_protection(&reopened, &gh, "develop", 86_500).unwrap();
        assert_eq!(reads(&gh), 4);
        // A newly chosen base must be checked even within the same cache period.
        cached_protection(&reopened, &gh, "release/topic", 86_501).unwrap();
        assert_eq!(reads(&gh), 5);
    }
    #[test]
    fn status_uses_refreshed_protection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let gh = backend(Ok(false));
        check_started_ticket(
            &store,
            &gh,
            &crate::herdr::fake::FakeHerdr::new(vec![]),
            "missing",
            1,
            true,
        )
        .unwrap_err();
        store
            .update(|data| {
                data.tickets.push(crate::store::Ticket {
                    id: "T1".into(),
                    base: "main".into(),
                    protection_warnings: vec![ProtectionWarning {
                        branch: "main".into(),
                        reason: "unprotected".into(),
                    }],
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        cached_protection(&store, &gh, "main", 100).unwrap();
        gh.protected.borrow_mut().insert("main".into(), Ok(true));
        cached_protection(&store, &gh, "main", 86_500).unwrap();
        assert!(status_warnings(&store.read().unwrap()).is_empty());
    }

    #[test]
    fn cache_changes_only_after_trust_refresh() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "--quiet", "-b", "main"],
            vec![
                "remote",
                "add",
                "origin",
                "https://example.invalid/first/repo.git",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .current_dir(dir.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        crate::trusted::ensure_snapshot(&dir.path().join("state"), dir.path()).unwrap();
        let gh = backend(Ok(true));
        assert!(
            cached_protection(&store, &gh, "main", 100)
                .unwrap()
                .is_empty()
        );
        gh.protected.borrow_mut().insert("main".into(), Ok(false));
        assert!(
            std::process::Command::new("git")
                .current_dir(dir.path())
                .args([
                    "remote",
                    "set-url",
                    "origin",
                    "https://example.invalid/second/repo.git"
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            cached_protection(&store, &gh, "main", 101)
                .unwrap()
                .is_empty()
        );
        crate::trusted::refresh_snapshot(&dir.path().join("state"), dir.path()).unwrap();
        assert_eq!(
            cached_protection(&store, &gh, "main", 102).unwrap().len(),
            1
        );
    }

    #[test]
    fn protection_notifications_honor_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        store
            .update(|data| {
                data.tickets.push(crate::store::Ticket {
                    id: "T1".into(),
                    base: "main".into(),
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let gh = backend(Ok(false));
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        check_started_ticket(&store, &gh, &h, "T1", 100, false).unwrap();
        assert_eq!(
            store.read().unwrap().tickets[0].protection_warnings[0].reason,
            "unprotected"
        );
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("notify "))
        );
    }

    #[test]
    fn gate_allows_this_read() {
        let args = protection_args("feature/base").unwrap();
        crate::gate::check_gh(&args.iter().map(String::as_str).collect::<Vec<_>>()).unwrap();
        assert_eq!(
            args,
            [
                "api",
                "-X",
                "GET",
                "repos/{owner}/{repo}/branches/feature/base",
                "--jq",
                ".protected"
            ]
        );
        for branch in ["../main", "a%2fb", "a?x", "--method=POST", "main\n"] {
            assert!(protection_args(branch).is_err(), "{branch:?}");
        }
    }
}
