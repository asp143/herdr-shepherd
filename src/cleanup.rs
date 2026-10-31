use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::Serialize;

use crate::{
    config::Config,
    error::ShepherdError,
    gate::{BranchPolicy, Gate},
    github::{PrBackend, PrState},
    herdr::{Herdr, Workspace},
    proc,
    upkeep::regroup,
};

/// Evidence is created only by a merged PR observation during preflight. The
/// private binding prevents reuse for another repository, branch, or tip.
pub struct MergeProof {
    pub pr: u64,
    pub merged_at: String,
    repo: PathBuf,
    branch: String,
    tip: String,
    base: String,
    origin: String,
    project_root: PathBuf,
    state_dir: PathBuf,
    base_tip: String,
    allow_local_for_tests: bool,
}

impl MergeProof {
    pub(crate) fn transport(&self) -> Result<crate::gate::transport::PushTransport, ShepherdError> {
        crate::gate::transport::PushTransport::open_with_options(
            &self.state_dir,
            &self.project_root,
            crate::gate::transport::TransportOptions {
                allow_local_for_tests: self.allow_local_for_tests,
            },
        )
    }
    fn validate_repository(&self, path: &Path) -> Result<(), ShepherdError> {
        let transport = self.transport()?;
        if transport.trust.common_git_dir != self.repo
            || transport.trust.push_url != self.origin
            || !transport.matches_repository(path)?
        {
            return Err(ShepherdError::PolicyDenied(
                "cleanup repository or trusted origin changed after merge proof".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn authorize(&self, gate: &Gate, branch: &str) -> Result<(), ShepherdError> {
        let denied =
            || ShepherdError::PolicyDenied("merge proof does not authorize this deletion".into());
        self.validate_repository(&gate.worktree)?;
        if branch != self.branch || branch != gate.ticket_branch || self.merged_at.is_empty() {
            return Err(denied());
        }
        let transport = self.transport()?;
        if transport
            .read_ref(&branch_ref(branch))?
            .is_some_and(|tip| tip != self.tip)
        {
            return Err(denied());
        }
        if !transport.is_ancestor(&self.tip, &self.base_tip)? {
            return Err(denied());
        }
        Ok(())
    }

    pub(crate) fn base_sync_target<'a>(
        &'a self,
        transport: &crate::gate::transport::PushTransport,
    ) -> Result<(&'a Path, &'a str, &'a str), ShepherdError> {
        if transport.trust.common_git_dir != self.repo
            || transport.trust.project_root != self.project_root
            || transport.trust.push_url != self.origin
            || transport.state_dir() != self.state_dir
            || self.pr == 0
            || self.merged_at.is_empty()
            || !transport.matches_repository(&self.project_root)?
        {
            return Err(ShepherdError::PolicyDenied(
                "merge proof does not authorize base synchronization".into(),
            ));
        }
        Ok((&self.project_root, &self.base, &self.base_tip))
    }

    pub(crate) fn tip(&self) -> &str {
        &self.tip
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteBranch {
    Deleted,
    AlreadyGone,
    Skipped(String),
}

#[derive(Debug, Serialize)]
pub struct CleanupReport {
    pub worktree: PathBuf,
    pub branch: String,
    pub pr: u64,
    pub dry_run: bool,
    pub workspace_removed: bool,
    pub local_branch_deleted: bool,
    pub local_branch_kept_reason: Option<String>,
    pub remote_branch: RemoteBranch,
    pub base_synced: Result<(), String>,
    pub children_removed: Vec<PathBuf>,
    pub ignored_files: Vec<PathBuf>,
}

struct Target {
    path: PathBuf,
    workspace: String,
    branch: String,
    tip: String,
}

pub struct Preflight<'a> {
    herdr: &'a dyn Herdr,
    cfg: &'a Config,
    main: PathBuf,
    target: Target,
    children: Vec<Target>,
    proof: MergeProof,
    policy: BranchPolicy,
    force_discard: bool,
    dry_run: bool,
}

fn capture(path: &Path, args: &[&str]) -> Result<proc::Captured, ShepherdError> {
    let mut command = Command::new("git");
    command.current_dir(path);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args);
    proc::capture(&mut command, Duration::from_secs(120), None).map_err(ShepherdError::Runtime)
}

fn git(path: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let out = capture(path, args)?;
    if !out.status.success() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "git failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(out.stdout.trim().into())
}

fn branch_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

fn repo_id(path: &Path) -> Result<PathBuf, ShepherdError> {
    let common = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    PathBuf::from(common)
        .canonicalize()
        .map_err(|error| error.into())
        .map_err(ShepherdError::Runtime)
}

fn ancestor(path: &Path, source: &str, base: &str) -> Result<bool, ShepherdError> {
    let out = capture(path, &["merge-base", "--is-ancestor", source, base])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(ShepherdError::Runtime(anyhow::anyhow!(
            "cannot prove branch containment: {}",
            out.stderr.trim()
        ))),
    }
}

fn check_caller(workspace: &str, caller: Option<&str>) -> Result<(), ShepherdError> {
    if caller == Some(workspace) {
        return Err(ShepherdError::Usage(
            "run cleanup from another workspace".into(),
        ));
    }
    Ok(())
}

fn owned_target(
    path: &Path,
    branch: &str,
    workspaces: &[Workspace],
    force_discard: bool,
    transport: &crate::gate::transport::PushTransport,
) -> Result<Target, ShepherdError> {
    if transport.ownership_marker(path)?.as_deref() != Some(branch) {
        return Err(ShepherdError::Usage(
            "worktree is not shepherd-owned (ticket branch marker missing or mismatched)".into(),
        ));
    }
    let matching: Vec<_> = workspaces
        .iter()
        .filter(|ws| {
            ws.worktree.as_ref().is_some_and(|wt| {
                wt.is_linked_worktree
                    && wt
                        .checkout_path
                        .as_ref()
                        .and_then(|path| path.canonicalize().ok())
                        .as_deref()
                        == Some(path)
            })
        })
        .collect();
    if matching.len() != 1 {
        return Err(ShepherdError::Usage(
            "worktree has no unique shepherd-owned workspace".into(),
        ));
    }
    let workspace = matching[0].workspace_id.clone();
    check_caller(
        &workspace,
        std::env::var("HERDR_WORKSPACE_ID").ok().as_deref(),
    )?;
    let tip = transport.checkout_head(path, branch)?;
    if !force_discard && !transport.checkout_status(path, branch)?.is_empty() {
        return Err(ShepherdError::Usage(format!(
            "worktree is dirty: {}",
            path.display()
        )));
    }
    Ok(Target {
        path: path.into(),
        workspace,
        branch: branch.into(),
        tip,
    })
}

fn worktrees(path: &Path) -> Result<Vec<(PathBuf, String)>, ShepherdError> {
    let output = git(path, &["worktree", "list", "--porcelain", "-z"])?;
    let mut trees = Vec::new();
    let mut current: Option<(PathBuf, String)> = None;
    for field in output.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(tree) = current.take() {
                trees.push(tree);
            }
            current = Some((PathBuf::from(path), String::new()));
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/")
            && let Some((_, current_branch)) = current.as_mut()
        {
            *current_branch = branch.into();
        }
    }
    if let Some(tree) = current {
        trees.push(tree);
    }
    Ok(trees)
}

fn is_child_branch(branch: &str, parent: &str) -> bool {
    branch
        .strip_prefix(&format!("{parent}--"))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn check_tree_set(
    trees: &[(PathBuf, String)],
    target: &Target,
    children: &[Target],
) -> Result<(), ShepherdError> {
    let owned: Vec<_> = std::iter::once(target).chain(children.iter()).collect();
    for expected in &owned {
        if !trees.iter().any(|(path, branch)| {
            path.canonicalize().ok().as_deref() == Some(expected.path.as_path())
                && branch == &expected.branch
        }) {
            return Err(ShepherdError::Usage(
                "worktree set changed after preflight".into(),
            ));
        }
    }
    for (path, branch) in trees {
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        let known = owned
            .iter()
            .any(|expected| expected.path == path && expected.branch == *branch);
        if !known && is_child_branch(branch, &target.branch) {
            return Err(ShepherdError::Usage(
                "child worktree set changed after preflight".into(),
            ));
        }
        if !known
            && owned
                .iter()
                .any(|expected| path != expected.path && path.starts_with(&expected.path))
        {
            return Err(ShepherdError::Usage(
                "unowned nested worktree blocks cleanup".into(),
            ));
        }
    }
    Ok(())
}

fn check_agents(h: &dyn Herdr, targets: &[&Target]) -> Result<(), ShepherdError> {
    for agent in h.agents()? {
        let cwd = agent
            .cwd
            .as_ref()
            .map(|cwd| cwd.canonicalize().unwrap_or_else(|_| cwd.clone()));
        if targets.iter().any(|target| {
            agent.workspace_id.as_deref() == Some(&target.workspace)
                || cwd
                    .as_ref()
                    .is_some_and(|cwd| cwd.starts_with(&target.path))
        }) {
            return Err(ShepherdError::Usage(
                "worktree still has agents; retire them before cleanup".into(),
            ));
        }
    }
    Ok(())
}

/// `dry_run` avoids fetch as well as mutations. Fetching would otherwise change
/// remote refs and make the promised read-only preview inaccurate.
pub fn preflight<'a>(
    h: &'a dyn Herdr,
    backend: &dyn PrBackend,
    cfg: &'a Config,
    worktree: &Path,
    force_discard: bool,
    dry_run: bool,
) -> Result<Preflight<'a>, ShepherdError> {
    preflight_with_state(
        h,
        backend,
        cfg,
        worktree,
        force_discard,
        dry_run,
        &crate::gate::state_dir(),
    )
}

pub fn preflight_with_state<'a>(
    h: &'a dyn Herdr,
    backend: &dyn PrBackend,
    cfg: &'a Config,
    worktree: &Path,
    force_discard: bool,
    dry_run: bool,
    state_dir: &Path,
) -> Result<Preflight<'a>, ShepherdError> {
    preflight_with_transport(
        h,
        backend,
        cfg,
        worktree,
        force_discard,
        dry_run,
        crate::gate::transport::PushTransport::open(state_dir, worktree)?,
    )
}

pub fn preflight_with_transport<'a>(
    h: &'a dyn Herdr,
    backend: &dyn PrBackend,
    cfg: &'a Config,
    worktree: &Path,
    force_discard: bool,
    dry_run: bool,
    transport: crate::gate::transport::PushTransport,
) -> Result<Preflight<'a>, ShepherdError> {
    let result = (|| {
        let worktree = worktree.canonicalize().map_err(anyhow::Error::from)?;
        let data = crate::store::Store::open(transport.state_dir(), &transport.trust.project_root)?
            .read()?;
        let branch = data
            .tickets
            .iter()
            .find(|ticket| ticket.worktree.as_deref() == Some(worktree.as_path()))
            .map(|ticket| ticket.branch.clone())
            .ok_or_else(|| {
                ShepherdError::PolicyDenied("cleanup requires a stored ticket worktree".into())
            })?;
        git(&worktree, &["check-ref-format", "--branch", &branch])?;
        let workspaces = h.workspaces()?;
        let target = owned_target(&worktree, &branch, &workspaces, force_discard, &transport)?;
        if transport.trust.origin_url != transport.trust.push_url {
            return Err(ShepherdError::PolicyDenied(
                "cleanup requires one snapshotted fetch/push origin".into(),
            ));
        }
        let origin = transport.trust.push_url.clone();
        let project_root = transport.trust.project_root.clone();
        let safe_tip = transport.verified_ticket_head(&branch)?;
        if safe_tip != target.tip {
            return Err(ShepherdError::PolicyDenied(
                "ticket HEAD changed during cleanup".into(),
            ));
        }
        let status = backend
            .view(&worktree, &branch)?
            .ok_or_else(|| ShepherdError::Usage("not merged: no PR".into()))?;
        if status.state != PrState::Merged {
            return Err(ShepherdError::Usage(format!(
                "not merged: {:?}",
                status.state
            )));
        }
        let merged_at = status
            .merged_at
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ShepherdError::Usage("merged PR has no merge timestamp".into()))?;
        if status.head != branch || status.number == 0 {
            return Err(ShepherdError::Usage(
                "merged PR does not match the ticket branch".into(),
            ));
        }
        let default = Some(transport.trust.default_branch.clone());
        let base = status.base.clone();
        git(&worktree, &["check-ref-format", "--branch", &base])?;
        let policy = BranchPolicy::new(
            &crate::gate::policy::extra_patterns(cfg)?,
            default.as_deref(),
            Some(&status.base),
        )?;
        if policy.is_protected(&branch) {
            return Err(ShepherdError::PolicyDenied(
                "cleanup cannot remove a protected branch".into(),
            ));
        }
        let base_tip = if dry_run {
            transport
                .remote_tip(&base)?
                .ok_or_else(|| ShepherdError::Usage("remote base is missing".into()))?
        } else {
            transport.fetch_branch(&base)?
        };
        if !transport.is_ancestor(&target.tip, &base_tip)?
            && !transport.patch_equivalent(&target.tip, &base_tip)?
            && status.head_oid.as_deref() != Some(target.tip.as_str())
        {
            return Err(ShepherdError::Usage(
                "ticket tip is not contained and does not match the merged PR head".into(),
            ));
        }
        let trees = worktrees(&worktree)?;
        let main = trees
            .first()
            .ok_or_else(|| ShepherdError::Usage("main checkout not found".into()))?
            .0
            .canonicalize()
            .map_err(anyhow::Error::from)?;
        if main == worktree {
            return Err(ShepherdError::Usage(
                "cleanup requires a linked worktree".into(),
            ));
        }
        let repo = repo_id(&worktree)?;
        if repo_id(&main)? != repo {
            return Err(ShepherdError::Usage(
                "main checkout belongs to a different repo".into(),
            ));
        }
        let mut children = Vec::new();
        for (path, child_branch) in &trees {
            if !is_child_branch(child_branch, &branch) {
                continue;
            }
            if policy.is_protected(child_branch) {
                return Err(ShepherdError::PolicyDenied(
                    "child branch is protected".into(),
                ));
            }
            let path = path.canonicalize().map_err(anyhow::Error::from)?;
            if repo_id(&path)? != repo {
                return Err(ShepherdError::Usage(
                    "child belongs to a different repo".into(),
                ));
            }
            let child = owned_target(&path, child_branch, &workspaces, force_discard, &transport)?;
            if !ancestor(&path, &child.tip, &target.tip)?
                && !transport.is_ancestor(&child.tip, &base_tip)?
                && !transport.patch_equivalent(&child.tip, &base_tip)?
            {
                return Err(ShepherdError::Usage(
                    "child commits are not contained in ticket or base".into(),
                ));
            }
            children.push(child);
        }
        children.sort_by_key(|child| std::cmp::Reverse(child.path.components().count()));
        check_tree_set(&trees, &target, &children)?;
        let mut targets = vec![&target];
        targets.extend(children.iter());
        check_agents(h, &targets)?;
        // A squash merge is sufficient to remove a clean workspace, but local
        // and remote branches are retained unless ancestry proves containment.
        let tip = target.tip.clone();
        Ok(Preflight {
            herdr: h,
            cfg,
            main,
            target,
            children,
            proof: MergeProof {
                pr: status.number,
                merged_at,
                repo,
                branch,
                tip,
                base,
                origin,
                project_root,
                state_dir: transport.state_dir().into(),
                allow_local_for_tests: transport.allow_local_for_tests(),
                base_tip,
            },
            policy,
            force_discard,
            dry_run,
        })
    })();
    if !dry_run && let Err(error) = &result {
        notify(h, cfg, "Shepherd cleanup blocked", &error.to_string());
    }
    result
}

fn notify(h: &dyn Herdr, cfg: &Config, title: &str, body: &str) {
    if cfg.upkeep.notifications
        && let Err(error) = h.notify(title, body)
    {
        eprintln!("shepherd: notification failed: {error}");
    }
}

fn base_sync_status(main: &Path, base: &str, proof: &MergeProof) -> Result<(), String> {
    let transport = proof
        .transport()
        .map_err(|_| "main_checkout_unavailable".to_owned())?;
    transport
        .checkout_head(main, base)
        .map_err(|_| "main_checkout_not_on_base".to_owned())?;
    match transport.checkout_status(main, base) {
        Ok(status) if status.is_empty() => (),
        Ok(_) => return Err("main_checkout_dirty".into()),
        Err(_) => return Err("main_checkout_unavailable".into()),
    }

    Ok(())
}

fn remote_state(
    proof: &MergeProof,
    branch: &str,
    tip: &str,
    contained: bool,
) -> Result<RemoteBranch, ShepherdError> {
    let transport = proof.transport()?;
    let Some(remote) = transport.remote_tip(branch)? else {
        return Ok(RemoteBranch::AlreadyGone);
    };
    if remote != tip {
        return Ok(RemoteBranch::Skipped("remote_branch_advanced".into()));
    }
    if !contained {
        return Ok(RemoteBranch::Skipped("branch_not_contained".into()));
    }
    Ok(RemoteBranch::Deleted)
}

fn delete_local(main: &Path, branch: &str, tip: &str, base: &str) -> Result<bool, ShepherdError> {
    let reference = branch_ref(branch);
    if git(main, &["rev-parse", "--verify", &reference])? != tip {
        return Err(ShepherdError::Usage("branch changed during cleanup".into()));
    }
    if !ancestor(main, tip, base)? {
        return Ok(false);
    }
    if worktrees(main)?
        .iter()
        .any(|(_, checked_out)| checked_out == branch)
    {
        return Err(ShepherdError::Usage(
            "branch is still checked out during cleanup".into(),
        ));
    }
    // A compare-and-delete transaction preserves a concurrently advanced ref.
    // branch -D cannot bind its deletion to the exact tip proved contained.
    git(main, &["update-ref", "-d", &reference, tip])?;
    Ok(true)
}

pub fn run(p: Preflight<'_>, dry_run: bool) -> Result<CleanupReport, ShepherdError> {
    if dry_run != p.dry_run {
        return Err(ShepherdError::Usage(
            "cleanup preflight and execution modes differ".into(),
        ));
    }
    let result = (|| {
        p.proof.validate_repository(&p.main)?;
        let contained = {
            let transport = p.proof.transport()?;
            transport.is_ancestor(&p.proof.tip, &p.proof.base_tip)?
        };
        let mut report = CleanupReport {
            worktree: p.target.path.clone(),
            branch: p.target.branch.clone(),
            pr: p.proof.pr,
            dry_run,
            workspace_removed: true,
            local_branch_deleted: contained,
            local_branch_kept_reason: (!contained).then(|| "branch_not_contained".into()),
            remote_branch: remote_state(&p.proof, &p.target.branch, &p.proof.tip, contained)?,
            base_synced: base_sync_status(&p.main, &p.proof.base, &p.proof),
            children_removed: p.children.iter().map(|child| child.path.clone()).collect(),
            ignored_files: p
                .children
                .iter()
                .chain(std::iter::once(&p.target))
                .map(|target| {
                    raw_ignored(&target.path).map(|paths| {
                        paths
                            .split('\0')
                            .filter(|s| !s.is_empty())
                            .map(|s| target.path.join(s))
                            .collect::<Vec<_>>()
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect(),
        };
        if dry_run {
            return Ok(report);
        }
        // Revalidate every target before the first removal, so a dirty or moved
        // child never leaves a half-cleaned set of worktrees.
        let workspaces = p.herdr.workspaces()?;
        check_tree_set(&worktrees(&p.main)?, &p.target, &p.children)?;
        for target in p.children.iter().chain(std::iter::once(&p.target)) {
            p.proof.validate_repository(&target.path)?;
            let current = {
                let transport = p.proof.transport()?;
                owned_target(
                    &target.path,
                    &target.branch,
                    &workspaces,
                    p.force_discard,
                    &transport,
                )?
            };
            if current.workspace != target.workspace {
                return Err(ShepherdError::Usage(
                    "worktree workspace changed after preflight".into(),
                ));
            }
            if current.tip != target.tip {
                return Err(ShepherdError::Usage(
                    "worktree tip changed after preflight".into(),
                ));
            }
        }
        let mut targets = vec![&p.target];
        targets.extend(p.children.iter());
        check_agents(p.herdr, &targets)?;
        for target in p.children.iter().chain(std::iter::once(&p.target)) {
            p.herdr
                .remove_worktree(&target.workspace, p.force_discard)?;
            if p.herdr
                .workspaces()?
                .iter()
                .any(|ws| ws.workspace_id == target.workspace)
            {
                p.herdr.close_workspace(&target.workspace)?;
            }
            if worktrees(&p.main)?
                .iter()
                .any(|(path, _)| path == &target.path)
            {
                return Err(ShepherdError::Runtime(anyhow::anyhow!(
                    "herdr did not remove the worktree"
                )));
            }
        }
        git(&p.main, &["worktree", "prune"])?;
        for child in &p.children {
            let _ = delete_local(&p.main, &child.branch, &child.tip, &p.proof.base_tip)?;
        }
        report.local_branch_deleted =
            delete_local(&p.main, &p.target.branch, &p.proof.tip, &p.proof.base_tip)?;
        report.local_branch_kept_reason =
            (!report.local_branch_deleted).then(|| "branch_not_contained".into());
        report.remote_branch = remote_state(&p.proof, &p.target.branch, &p.proof.tip, contained)?;
        if report.remote_branch == RemoteBranch::Deleted {
            let gate = Gate {
                policy: p.policy,
                ticket_branch: p.target.branch.clone(),
                worktree: p.main.clone(),
                state_dir: p.proof.state_dir.clone(),
                config_dir: crate::config::resolve_config_dir(None),
            };
            gate.delete_remote_branch(&p.target.branch, &p.proof)?;
        }
        report.base_synced = base_sync_status(&p.main, &p.proof.base, &p.proof);
        if report.base_synced.is_ok() {
            report.base_synced = p
                .proof
                .transport()?
                .sync_merged_base(&p.proof)
                .map_err(|error| error.to_string());
        }
        if p.cfg.upkeep.regroup {
            regroup::apply(p.herdr, &regroup::plan_regroup(&p.herdr.workspaces()?))?;
        }
        Ok(report)
    })();
    if !dry_run {
        match &result {
            Ok(report) => {
                notify(
                    p.herdr,
                    p.cfg,
                    "Shepherd cleanup complete",
                    &p.target.branch,
                );
                crate::hooks::emit(
                    p.cfg,
                    "ticket.cleaned_up",
                    &serde_json::json!({
                        "worktree": report.worktree, "branch": report.branch, "pr": report.pr,
                    }),
                );
            }
            Err(error) => notify(
                p.herdr,
                p.cfg,
                "Shepherd cleanup blocked",
                &error.to_string(),
            ),
        }
    }
    result
}

fn raw_ignored(path: &Path) -> Result<String, ShepherdError> {
    let out = capture(
        path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    if !out.status.success() {
        return Err(anyhow::anyhow!("cannot enumerate ignored files").into());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_self_workspace() {
        let error = check_caller("ticket", Some("ticket")).unwrap_err();
        assert!(
            matches!(error, ShepherdError::Usage(message) if message == "run cleanup from another workspace")
        );
        check_caller("ticket", Some("other")).unwrap();
    }
}
