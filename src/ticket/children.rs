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
    for (index, child) in pending {
        let range = format!("{}..{}", child.base_sha, child.branch);
        let already_contained = git(
            ticket_wt,
            &[
                "merge-base",
                "--is-ancestor",
                &format!("refs/heads/{}", child.branch),
                "HEAD",
            ],
        )
        .is_ok();
        if !already_contained
            && git(ticket_wt, &["rev-list", "--count", &range])? != "0"
            && git(
                ticket_wt,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "cherry-pick",
                    "--no-edit",
                    &range,
                ],
            )
            .is_err()
        {
            // Abort the current sequencer before undoing earlier units in this
            // transaction. No child is marked integrated until all picks pass.
            let _ = git(ticket_wt, &["cherry-pick", "--abort"]);
            git(ticket_wt, &["reset", "--hard", &original_head])?;
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
    let workspace = h.create_worktree(
        source,
        &branch,
        &format!("{}-{unit}", ticket.slug),
        &base_sha,
        &path,
    )?;
    let path = path.canonicalize().map_err(anyhow::Error::from)?;
    let child = ChildWorktree {
        unit,
        branch,
        path,
        workspace_id: workspace.workspace_id,
        base_sha,
        integrated: false,
    };
    if let Err(error) =
        hook::install_configured(&child.path, &child.branch, state_dir, bin, config_dir)
    {
        if let Err(cleanup) = rollback_created_child(h, ticket_wt, &child) {
            let recovery = record_failed_child(&child)?;
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "child hook installation failed: {error}; cleanup failed: {cleanup}; ownership recovery saved at {}",
                recovery.display()
            )));
        }
        return Err(error);
    }
    Ok(child)
}

/// Remove integrated child resources after proving their commits are present.
///
/// The whole set is checked before the first removal. Worktree removal never
/// forces dirty files away, and ref deletion compares the observed tip.
pub fn cleanup_integrated(
    h: &dyn Herdr,
    ticket_wt: &Path,
    children: &[ChildWorktree],
) -> Result<Vec<u32>, ShepherdError> {
    let _lock = integration_lock(ticket_wt)?;
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
                            == Some(&child_path)
                })
            })
            .collect();
        if matching.len() != 1 || matching[0].workspace_id != child.workspace_id {
            return Err(ShepherdError::PolicyDenied(
                "child workspace ownership changed".into(),
            ));
        }
        if agents.iter().any(|agent| {
            !matches!(agent.agent_status.as_str(), "exited" | "stopped")
                && (agent.workspace_id.as_deref() == Some(&child.workspace_id)
                    || agent
                        .cwd
                        .as_ref()
                        .and_then(|path| path.canonicalize().ok())
                        .is_some_and(|path| path.starts_with(&child_path)))
        }) {
            return Err(ShepherdError::Usage("child still has a live agent".into()));
        }
        let tip = git(&child.path, &["rev-parse", "HEAD"])?;
        let reference = format!("refs/heads/{}", child.branch);
        if git(ticket_wt, &["rev-parse", "--verify", &reference])? != tip {
            return Err(ShepherdError::PolicyDenied(
                "child branch tip changed".into(),
            ));
        }
        let patches = git(ticket_wt, &["cherry", &ticket_head, &tip, &child.base_sha])?;
        if patches.lines().any(|line| !line.starts_with("- ")) {
            return Err(ShepherdError::PolicyDenied(
                "child commits are not integrated".into(),
            ));
        }
        targets.push((child, reference, tip));
    }
    let mut removed = Vec::new();
    for (child, reference, tip) in targets {
        // Recheck immediately before each effect; Git itself refuses dirty
        // worktrees, and the expected-tip deletion preserves advanced refs.
        if git(&child.path, &["rev-parse", "HEAD"])? != tip
            || !git(&child.path, &["status", "--porcelain"])?.is_empty()
        {
            return Err(ShepherdError::Usage(
                "child changed after cleanup preflight".into(),
            ));
        }
        h.remove_worktree(&child.workspace_id, false)?;
        if h.workspaces()?
            .iter()
            .any(|workspace| workspace.workspace_id == child.workspace_id)
        {
            h.close_workspace(&child.workspace_id)?;
        }
        if child.path.exists() {
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "herdr did not remove child worktree"
            )));
        }
        git(ticket_wt, &["update-ref", "-d", &reference, &tip])?;
        removed.push(child.unit);
    }
    Ok(removed)
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
    let workspaces = h.workspaces()?;
    let owners: Vec<_> = workspaces
        .iter()
        .filter(|workspace| {
            workspace.worktree.as_ref().is_some_and(|tree| {
                tree.is_linked_worktree
                    && tree
                        .checkout_path
                        .as_ref()
                        .and_then(|path| path.canonicalize().ok())
                        .as_deref()
                        == Some(&path)
            })
        })
        .collect();
    if owners.len() != 1 || owners[0].workspace_id != child.workspace_id {
        return Err(ShepherdError::PolicyDenied(
            "new child workspace ownership changed".into(),
        ));
    }
    if h.agents()?.iter().any(|agent| {
        !matches!(agent.agent_status.as_str(), "exited" | "stopped")
            && (agent.workspace_id.as_deref() == Some(&child.workspace_id)
                || agent
                    .cwd
                    .as_ref()
                    .and_then(|path| path.canonicalize().ok())
                    .is_some_and(|cwd| cwd.starts_with(&path)))
    }) {
        return Err(ShepherdError::Usage("new child has a live agent".into()));
    }
    h.remove_worktree(&child.workspace_id, false)?;
    if h.workspaces()?
        .iter()
        .any(|workspace| workspace.workspace_id == child.workspace_id)
    {
        h.close_workspace(&child.workspace_id)?;
    }
    if child.path.exists() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "herdr did not remove new child worktree"
        )));
    }
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
