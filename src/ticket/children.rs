//! Integrate independently committed child units into a ticket branch.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::Serialize;

mod cleanup;
pub use cleanup::{cleanup_integrated_recorded, cleanup_integrated_recorded_with_config};

use crate::{
    error::ShepherdError,
    gate::hook,
    herdr::Herdr,
    proc,
    store::{ChildWorktree, Ticket, project_hash},
};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IntegrateReport {
    pub units: Vec<u32>,
    pub ranges: Vec<String>,
    pub head: String,
}

fn git(path: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let output = proc::capture(
        Command::new("git").current_dir(path).args(args),
        Duration::from_secs(30),
        None,
    )?;
    if !output.status.success() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "git {} failed: {}",
            args.join(" "),
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.trim().into())
}

fn integration_lock(ticket_wt: &Path) -> Result<File, ShepherdError> {
    let git_dir = git(ticket_wt, &["rev-parse", "--absolute-git-dir"])?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(Path::new(&git_dir).join("shepherd-integration.lock"))
        .map_err(anyhow::Error::from)?;
    file.lock().map_err(anyhow::Error::from)?;
    Ok(file)
}

/// Return each pending child's recorded commit range in integration order.
pub fn conflict_ranges(children: &[ChildWorktree]) -> Vec<String> {
    let mut children: Vec<_> = children.iter().filter(|child| !child.integrated).collect();
    children.sort_by_key(|child| child.unit);
    children
        .into_iter()
        .map(|child| format!("{}..{}", child.base_sha, child.branch))
        .collect()
}

fn rollback_integration(
    ticket_wt: &Path,
    ticket_ref: &str,
    original_head: &str,
    owned_heads: &HashSet<String>,
) -> Result<(), ShepherdError> {
    let recovery = |reason: String| {
        ShepherdError::Runtime(anyhow::anyhow!(
            "integration rollback refused: {reason}; no hard reset performed. \
             To recover, preserve local edits, inspect git status, and restore \
             {ticket_ref} from {original_head} after resolving or aborting the merge"
        ))
    };
    let check_owner = || -> Result<(), ShepherdError> {
        let current_ref = git(ticket_wt, &["symbolic-ref", "--quiet", "HEAD"])
            .map_err(|error| recovery(format!("cannot verify HEAD branch: {error}")))?;
        let head = git(ticket_wt, &["rev-parse", "HEAD"])
            .map_err(|error| recovery(format!("cannot verify HEAD tip: {error}")))?;
        if current_ref != ticket_ref || !owned_heads.contains(&head) {
            return Err(recovery(format!(
                "HEAD changed to {current_ref} at {head}, expected {ticket_ref} at an integration-owned tip"
            )));
        }
        Ok(())
    };
    // Abort also moves refs and files, so check ownership before either step.
    check_owner()?;
    let git_dir = git(ticket_wt, &["rev-parse", "--absolute-git-dir"])?;
    if Path::new(&git_dir).join("MERGE_HEAD").exists() {
        git(
            ticket_wt,
            &["-c", "core.hooksPath=/dev/null", "merge", "--abort"],
        )
        .map_err(|error| recovery(format!("merge --abort failed: {error}")))?;
    }
    if !git(ticket_wt, &["status", "--porcelain"])?.is_empty() {
        return Err(recovery("worktree has edits after the failed merge".into()));
    }
    check_owner()?;
    git(
        ticket_wt,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "reset",
            "--hard",
            original_head,
        ],
    )?;
    Ok(())
}

/// Integrate pending units in ascending order and mark them only after success.
pub fn integrate(
    ticket_wt: &Path,
    children: &mut [ChildWorktree],
) -> Result<IntegrateReport, ShepherdError> {
    let _lock = integration_lock(ticket_wt)?;
    if !git(ticket_wt, &["status", "--porcelain"])?.is_empty() {
        return Err(ShepherdError::Usage(
            "ticket worktree must be clean before integration".into(),
        ));
    }
    let git_dir = git(ticket_wt, &["rev-parse", "--absolute-git-dir"])?;
    if [
        "CHERRY_PICK_HEAD",
        "MERGE_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
        "sequencer",
    ]
    .iter()
    .any(|marker| Path::new(&git_dir).join(marker).exists())
    {
        return Err(ShepherdError::Usage(
            "finish the existing Git operation before integration".into(),
        ));
    }
    let original_head = git(ticket_wt, &["rev-parse", "HEAD"])?;
    let mut pending: Vec<_> = children
        .iter()
        .enumerate()
        .filter(|(_, child)| !child.integrated)
        .collect();
    pending.sort_by_key(|(_, child)| child.unit);
    let common = git(
        ticket_wt,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let ticket_branch = git(ticket_wt, &["branch", "--show-current"])?;
    let ticket_ref = format!("refs/heads/{ticket_branch}");
    let mut seen = HashSet::new();
    for (_, child) in &pending {
        if child.unit == 0
            || !seen.insert(child.unit)
            || child.branch != format!("{ticket_branch}--{}", child.unit)
            || git(&child.path, &["branch", "--show-current"])? != child.branch
            || git(
                &child.path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )? != common
        {
            return Err(ShepherdError::PolicyDenied(
                "child does not belong to this ticket repository and branch".into(),
            ));
        }
        git(ticket_wt, &["check-ref-format", "--branch", &child.branch])?;
        if !matches!(child.base_sha.len(), 40 | 64)
            || !child.base_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ShepherdError::Usage(
                "child base must be a full commit id".into(),
            ));
        }
        git(
            ticket_wt,
            &[
                "merge-base",
                "--is-ancestor",
                &child.base_sha,
                &format!("refs/heads/{}", child.branch),
            ],
        )?;
    }
    let mut units = Vec::new();
    let mut ranges = Vec::new();
    let mut indices = Vec::new();
    let mut owned_heads = HashSet::from([original_head.clone()]);
    let mut previous_head = original_head.clone();
    for (index, child) in pending {
        let range = format!("{}..{}", child.base_sha, child.branch);
        let tip = git(ticket_wt, &["rev-parse", &child.branch])?;
        // Branch defaults can accumulate strategies/-X options or suppress the
        // commit while still claiming ancestry. Pin transactional semantics;
        // project attributes and their merge drivers remain in effect.
        if git(
            ticket_wt,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                &format!("branch.{ticket_branch}.mergeOptions="),
                "merge",
                "--strategy=ort",
                "--no-ff",
                "--no-edit",
                "--no-squash",
                "--no-autostash",
                "--no-verify",
                "--commit",
                &child.branch,
            ],
        )
        .and_then(|_| {
            let head = git(ticket_wt, &["rev-parse", "HEAD"])?;
            // Only record a tip this invocation actually produced, never an
            // unrelated commit written concurrently while merge was running.
            if git(ticket_wt, &["symbolic-ref", "--quiet", "HEAD"])? != ticket_ref
                || (head != previous_head
                    && git(ticket_wt, &["rev-list", "--parents", "-1", &head])?
                        != format!("{head} {previous_head} {tip}"))
            {
                return Err(ShepherdError::Runtime(anyhow::anyhow!(
                    "HEAD changed during integration"
                )));
            }
            owned_heads.insert(head.clone());
            previous_head = head;
            git(ticket_wt, &["merge-base", "--is-ancestor", &tip, "HEAD"])
        })
        .is_err()
        {
            // A later unit must not leave earlier merges from this invocation.
            rollback_integration(ticket_wt, &ticket_ref, &original_head, &owned_heads)?;
            return Err(ShepherdError::Conflict {
                unit: child.unit,
                range,
            });
        }
        units.push(child.unit);
        ranges.push(range);
        indices.push(index);
    }
    let head = git(ticket_wt, &["rev-parse", "HEAD"])?;
    for index in indices {
        children[index].integrated = true;
    }
    Ok(IntegrateReport {
        units,
        ranges,
        head,
    })
}

/// Create an owned parallel unit at the ticket branch's current tip.
pub fn create_child(
    h: &dyn Herdr,
    ticket: &Ticket,
    unit: u32,
    state_dir: &Path,
    bin: &Path,
) -> Result<ChildWorktree, ShepherdError> {
    create_child_configured(
        h,
        ticket,
        unit,
        state_dir,
        bin,
        &crate::config::resolve_config_dir(None),
    )
}

/// Create a child whose first push guard uses the selected global config.
pub fn create_child_configured(
    h: &dyn Herdr,
    ticket: &Ticket,
    unit: u32,
    state_dir: &Path,
    bin: &Path,
    config_dir: &Path,
) -> Result<ChildWorktree, ShepherdError> {
    if unit == 0 || ticket.children.iter().any(|child| child.unit == unit) {
        return Err(ShepherdError::Usage(
            "child unit must be positive and unique".into(),
        ));
    }
    let ticket_wt = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    if git(ticket_wt, &["branch", "--show-current"])? != ticket.branch {
        return Err(ShepherdError::Usage(
            "ticket worktree is on a different branch".into(),
        ));
    }
    let _lock = integration_lock(ticket_wt)?;
    let base_sha = git(ticket_wt, &["rev-parse", "HEAD"])?;
    let branch = format!("{}--{unit}", ticket.branch);
    git(ticket_wt, &["check-ref-format", "--branch", &branch])?;
    let path = state_dir
        .join("children")
        .join(project_hash(ticket_wt))
        .join(unit.to_string());
    std::fs::create_dir_all(path.parent().expect("child has parent"))
        .map_err(anyhow::Error::from)?;
    // Herdr rejects a linked checkout as the source of another worktree.
    // Git lists the primary checkout first, including separate-git-dir layouts.
    // Keep the base pinned to the ticket tip rather than the primary branch.
    let trees = git(ticket_wt, &["worktree", "list", "--porcelain", "-z"])?;
    let source = trees
        .split('\0')
        .next()
        .and_then(|field| field.strip_prefix("worktree "))
        .ok_or_else(|| ShepherdError::Usage("repository has no primary worktree".into()))?;
    let source = Path::new(source);
    if branch_exists(ticket_wt, &branch)? {
        return Err(ShepherdError::Usage("child branch already exists".into()));
    }
    let path = path
        .parent()
        .expect("child has parent")
        .canonicalize()
        .map_err(anyhow::Error::from)?
        .join(unit.to_string());
    let path_arg = path
        .to_str()
        .ok_or_else(|| ShepherdError::Usage("child path is not UTF-8".into()))?
        .to_owned();
    // Reserve an empty destination before Git can create its branch. In
    // particular, an occupied path must never leave a newly created ref.
    std::fs::create_dir(&path).map_err(anyhow::Error::from)?;
    let child = ChildWorktree {
        unit,
        branch,
        path,
        workspace_id: None,
        base_sha,
        integrated: false,
    };
    let setup = git(
        source,
        &[
            "worktree",
            "add",
            "-b",
            &child.branch,
            &path_arg,
            &child.base_sha,
        ],
    )
    .and_then(|_| hook::install_configured(&child.path, &child.branch, state_dir, bin, config_dir));
    if let Err(error) = setup {
        // A failure before Git allocated anything owns only our empty
        // reservation. Never delete a preexisting or concurrently moved ref.
        if matches!(branch_exists(ticket_wt, &child.branch), Ok(false))
            && std::fs::remove_dir(&child.path).is_ok()
        {
            return Err(error);
        }
        if let Err(cleanup) = rollback_created_child(h, ticket_wt, &child) {
            let recovery = record_failed_child(&child)?;
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "child setup failed: {error}; cleanup failed: {cleanup}; ownership recovery saved at {}",
                recovery.display()
            )));
        }
        return Err(error);
    }
    Ok(child)
}

/// Remove integrated child resources after proving their commits landed.
///
/// The whole set is checked before the first removal. Worktree removal never
/// forces dirty files away, and ref deletion compares the observed tip.
pub fn cleanup_integrated(
    h: &dyn Herdr,
    ticket_wt: &Path,
    children: &[ChildWorktree],
) -> Result<Vec<u32>, ShepherdError> {
    let _lock = integration_lock(ticket_wt)?;
    let targets = preflight_integrated(h, ticket_wt, children, false)?;
    let mut removed = Vec::new();
    for receipt in targets {
        let child = &receipt.child;
        verify_workspace_owner(&h.workspaces()?, child, &receipt.path)?;
        ensure_no_live_agents(h, child)?;
        verify_child_tip(ticket_wt, child, &receipt.tip, true)?;
        remove_child_checkout(h, ticket_wt, child)?;
        git(
            ticket_wt,
            &[
                "update-ref",
                "-d",
                &format!("refs/heads/{}", child.branch),
                &receipt.tip,
            ],
        )?;
        removed.push(child.unit);
    }
    Ok(removed)
}

fn preflight_integrated(
    h: &dyn Herdr,
    ticket_wt: &Path,
    children: &[ChildWorktree],
    allow_agents: bool,
) -> Result<Vec<crate::store::ChildCleanup>, ShepherdError> {
    if children.iter().any(|child| !child.integrated) {
        return Err(ShepherdError::Usage(
            "integrate every child before cleanup".into(),
        ));
    }
    let ticket_branch = git(ticket_wt, &["branch", "--show-current"])?;
    let ticket_head = git(ticket_wt, &["rev-parse", "HEAD"])?;
    let common = git(
        ticket_wt,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let workspaces = h.workspaces()?;
    let agents = h.agents()?;
    let mut targets = Vec::new();
    for child in children {
        if child.branch != format!("{ticket_branch}--{}", child.unit)
            || git(&child.path, &["branch", "--show-current"])? != child.branch
            || git(
                &child.path,
                &["config", "--worktree", "--get", "shepherd.ticketBranch"],
            )? != child.branch
            || git(
                &child.path,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )? != common
        {
            return Err(ShepherdError::PolicyDenied(
                "child worktree ownership changed".into(),
            ));
        }
        if !git(&child.path, &["status", "--porcelain"])?.is_empty() {
            return Err(ShepherdError::Usage(
                "child worktree must be clean before cleanup".into(),
            ));
        }
        let child_path = child.path.canonicalize().map_err(anyhow::Error::from)?;
        verify_workspace_owner(&workspaces, child, &child_path)?;
        if !allow_agents
            && agents.iter().any(|agent| {
                !matches!(agent.agent_status.as_str(), "exited" | "stopped")
                    && (child
                        .workspace_id
                        .as_ref()
                        .is_some_and(|id| agent.workspace_id.as_ref() == Some(id))
                        || agent
                            .cwd
                            .as_ref()
                            .and_then(|path| path.canonicalize().ok())
                            .is_some_and(|path| path.starts_with(&child_path)))
            })
        {
            return Err(ShepherdError::Usage("child still has a live agent".into()));
        }
        let tip = git(&child.path, &["rev-parse", "HEAD"])?;
        let reference = format!("refs/heads/{}", child.branch);
        if git(ticket_wt, &["rev-parse", "--verify", &reference])? != tip {
            return Err(ShepherdError::PolicyDenied(
                "child branch tip changed".into(),
            ));
        }
        verify_integrated_tip(ticket_wt, child, &tip, &ticket_head)?;
        targets.push(crate::store::ChildCleanup {
            child: child.clone(),
            path: child_path,
            tip,
            retiring_agents: Vec::new(),
        });
    }
    Ok(targets)
}

fn verify_integrated_tip(
    ticket_wt: &Path,
    child: &ChildWorktree,
    tip: &str,
    head: &str,
) -> Result<(), ShepherdError> {
    if !matches!(child.base_sha.len(), 40 | 64)
        || !child.base_sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(ShepherdError::PolicyDenied("invalid child base".into()));
    }
    git(
        ticket_wt,
        &["merge-base", "--is-ancestor", &child.base_sha, tip],
    )?;
    git(ticket_wt, &["diff", "--cached", "--quiet", head])?;
    if git(ticket_wt, &["merge-base", "--is-ancestor", tip, head]).is_err() {
        return Err(ShepherdError::PolicyDenied(format!(
            "child commits are not integrated; child tip is not an ancestor of ticket HEAD. Review and merge {} into the ticket branch, then retry cleanup",
            child.branch
        )));
    }
    Ok(())
}

// This path only removes a child created by the current call, before agents
// are launched. Any change to its branch, tree, tip, or ownership keeps it.
fn rollback_created_child(
    h: &dyn Herdr,
    ticket_wt: &Path,
    child: &ChildWorktree,
) -> Result<(), ShepherdError> {
    let common_args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    if git(&child.path, &["branch", "--show-current"])? != child.branch
        || git(&child.path, &["rev-parse", "HEAD"])? != child.base_sha
        || git(&child.path, &common_args)? != git(ticket_wt, &common_args)?
        || !git(&child.path, &["status", "--porcelain"])?.is_empty()
    {
        return Err(ShepherdError::PolicyDenied(
            "new child changed before rollback".into(),
        ));
    }
    let path = child.path.canonicalize().map_err(anyhow::Error::from)?;
    verify_workspace_owner(&h.workspaces()?, child, &path)?;
    ensure_no_live_agents(h, child)?;
    verify_child_tip(ticket_wt, child, &child.base_sha, false)?;
    remove_child_checkout(h, ticket_wt, child)?;
    git(
        ticket_wt,
        &[
            "update-ref",
            "-d",
            &format!("refs/heads/{}", child.branch),
            &child.base_sha,
        ],
    )?;
    Ok(())
}

fn record_failed_child(child: &ChildWorktree) -> Result<PathBuf, ShepherdError> {
    let recovery = child.path.with_extension("pending.json");
    let temporary = child.path.with_extension("pending.json.tmp");
    let bytes = serde_json::to_vec_pretty(child).map_err(anyhow::Error::from)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary)
        .map_err(anyhow::Error::from)?;
    file.write_all(&bytes).map_err(anyhow::Error::from)?;
    file.sync_all().map_err(anyhow::Error::from)?;
    std::fs::rename(&temporary, &recovery).map_err(anyhow::Error::from)?;
    if let Some(parent) = recovery.parent() {
        File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(anyhow::Error::from)?;
    }
    Ok(recovery)
}

fn verify_workspace_owner(
    workspaces: &[crate::herdr::Workspace],
    child: &ChildWorktree,
    path: &Path,
) -> Result<(), ShepherdError> {
    let matching: Vec<_> = workspaces
        .iter()
        .filter(|workspace| {
            workspace.worktree.as_ref().is_some_and(|tree| {
                tree.is_linked_worktree
                    && tree
                        .checkout_path
                        .as_ref()
                        .and_then(|path| path.canonicalize().ok())
                        .as_deref()
                        == Some(path)
            })
        })
        .collect();
    let owned = match &child.workspace_id {
        Some(id) => matching.len() == 1 && matching[0].workspace_id == *id,
        None => matching.is_empty(),
    };
    if !owned {
        return Err(ShepherdError::PolicyDenied(
            "child workspace ownership changed".into(),
        ));
    }
    Ok(())
}

fn ensure_no_live_agents(h: &dyn Herdr, child: &ChildWorktree) -> Result<(), ShepherdError> {
    let path = child.path.canonicalize().map_err(anyhow::Error::from)?;
    if h.agents()?.iter().any(|agent| {
        !matches!(agent.agent_status.as_str(), "exited" | "stopped")
            && (child
                .workspace_id
                .as_ref()
                .is_some_and(|id| agent.workspace_id.as_ref() == Some(id))
                || agent
                    .cwd
                    .as_ref()
                    .and_then(|cwd| cwd.canonicalize().ok())
                    .is_some_and(|cwd| cwd.starts_with(&path)))
    }) {
        return Err(ShepherdError::Usage("child still has a live agent".into()));
    }
    Ok(())
}

// Revalidate after observing Herdr state and immediately before removing a
// checkout. In particular, an advanced ref must retain both tree and branch.
fn verify_child_tip(
    ticket_wt: &Path,
    child: &ChildWorktree,
    tip: &str,
    guarded: bool,
) -> Result<(), ShepherdError> {
    let common_args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = git(ticket_wt, &common_args)?;
    if git(&child.path, &["branch", "--show-current"])? != child.branch
        || git(&child.path, &["rev-parse", "HEAD"])? != tip
        || git(
            ticket_wt,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}", child.branch),
            ],
        )? != tip
        || git(&child.path, &common_args)? != common
        || git(&child.path, &["rev-parse", "--absolute-git-dir"])? == common
        || !git(&child.path, &["status", "--porcelain"])?.is_empty()
        || (guarded
            && git(
                &child.path,
                &["config", "--worktree", "--get", "shepherd.ticketBranch"],
            )? != child.branch)
    {
        return Err(ShepherdError::PolicyDenied(
            "child changed before removal".into(),
        ));
    }
    Ok(())
}

fn remove_child_checkout(
    h: &dyn Herdr,
    ticket_wt: &Path,
    child: &ChildWorktree,
) -> Result<(), ShepherdError> {
    if let Some(id) = &child.workspace_id {
        h.remove_worktree(id, false)?;
        if h.workspaces()?
            .iter()
            .any(|workspace| workspace.workspace_id == *id)
        {
            h.close_workspace(id)?;
        }
    } else {
        git(
            ticket_wt,
            &[
                "worktree",
                "remove",
                "--",
                child
                    .path
                    .to_str()
                    .ok_or_else(|| ShepherdError::Usage("child path is not UTF-8".into()))?,
            ],
        )?;
    }
    if child.path.exists() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "child worktree was not removed"
        )));
    }
    Ok(())
}

fn branch_exists(worktree: &Path, branch: &str) -> Result<bool, ShepherdError> {
    Ok(!git(
        worktree,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/heads/{branch}"),
        ],
    )?
    .is_empty())
}
