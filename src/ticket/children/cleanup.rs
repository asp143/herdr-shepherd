//! Durable integration cleanup. The existing child_cleanup receipt spans every
//! destructive effect, including retirement, checkout removal and ref deletion.
use super::*;
use crate::store::{AgentStatus, ChildCleanup, Store};

fn denied() -> ShepherdError {
    ShepherdError::PolicyDenied("integration cleanup ownership changed".into())
}

// Permit independently registered children, but never replace or erase them.
fn recorded<'a>(
    data: &'a crate::store::StoreData,
    expected: &Ticket,
    children: &[ChildWorktree],
) -> Result<&'a Ticket, ShepherdError> {
    let current = data
        .tickets
        .iter()
        .find(|t| t.id == expected.id)
        .ok_or_else(denied)?;
    if current.branch != expected.branch
        || current.worktree != expected.worktree
        || current.workspace_id != expected.workspace_id
        || children
            .iter()
            .any(|child| current.children.iter().filter(|c| *c == child).count() != 1)
    {
        return Err(denied());
    }
    Ok(current)
}

fn validate_receipt(
    ticket: &Ticket,
    receipt: &ChildCleanup,
    agents: &[crate::store::AgentRecord],
) -> Result<(), ShepherdError> {
    if !receipt.child.integrated
        || receipt.child.unit == 0
        || receipt.child.branch != format!("{}--{}", ticket.branch, receipt.child.unit)
        || receipt.child.workspace_id.is_some() && receipt.child.workspace_id == ticket.workspace_id
        || ticket
            .children
            .iter()
            .filter(|c| **c == receipt.child)
            .count()
            != 1
        || ticket
            .child_cleanup
            .iter()
            .filter(|r| r.child.unit == receipt.child.unit)
            .count()
            != 1
        || !matches!(receipt.tip.len(), 40 | 64)
        || !receipt.tip.bytes().all(|b| b.is_ascii_hexdigit())
        || receipt.retiring_agents.iter().any(|a| {
            let mut retired = a.clone();
            retired.status = AgentStatus::Retired;
            a.ticket != ticket.id
                || a.status != AgentStatus::Reported
                || !agents
                    .iter()
                    .any(|current| *current == *a || *current == retired)
        })
    {
        return Err(denied());
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool, ShepherdError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::Error::from(e).into()),
    }
}

// Canonicalize the parent even when the checkout is gone. An arbitrary missing
// directory, reused branch, symlink, or reassigned legacy handle is not proof.
fn validate_removed(
    h: &dyn Herdr,
    ticket_wt: &Path,
    receipt: &ChildCleanup,
) -> Result<(), ShepherdError> {
    let child = &receipt.child;
    let parent = child
        .path
        .parent()
        .ok_or_else(denied)?
        .canonicalize()
        .map_err(anyhow::Error::from)?;
    if parent.join(child.path.file_name().ok_or_else(denied)?) != receipt.path
        || exists(&receipt.path)?
    {
        return Err(denied());
    }
    let trees = git(ticket_wt, &["worktree", "list", "--porcelain", "-z"])?;
    if trees.split('\0').any(|field| {
        field
            .strip_prefix("worktree ")
            .is_some_and(|p| Path::new(p) == receipt.path)
            || field == format!("branch refs/heads/{}", child.branch)
    }) {
        return Err(denied());
    }
    if branch_exists(ticket_wt, &child.branch)?
        && git(
            ticket_wt,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}", child.branch),
            ],
        )? != receipt.tip
    {
        return Err(denied());
    }
    let common_args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let common = git(ticket_wt, &common_args)?;
    for ws in h.workspaces()? {
        let same_id = child.workspace_id.as_ref() == Some(&ws.workspace_id);
        let same_path = ws
            .worktree
            .as_ref()
            .is_some_and(|wt| wt.checkout_path.as_ref() == Some(&receipt.path));
        if !same_id && !same_path {
            continue;
        }
        let wt = ws.worktree.as_ref().ok_or_else(denied)?;
        if !same_id
            || !same_path
            || !wt.is_linked_worktree
            || wt
                .repo_root
                .as_ref()
                .is_none_or(|root| git(root, &common_args).ok().as_ref() != Some(&common))
            || std::env::var("HERDR_WORKSPACE_ID").ok().as_ref() == Some(&ws.workspace_id)
        {
            return Err(denied());
        }
    }
    if h.agents()?.iter().any(|a| {
        !matches!(a.agent_status.as_str(), "exited" | "stopped")
            && (child
                .workspace_id
                .as_ref()
                .is_some_and(|id| a.workspace_id.as_ref() == Some(id))
                || a.cwd
                    .as_ref()
                    .is_some_and(|cwd| cwd.starts_with(&receipt.path)))
    }) {
        return Err(ShepherdError::Usage("child still has a live agent".into()));
    }
    Ok(())
}

fn validate_target(
    h: &dyn Herdr,
    ticket_wt: &Path,
    receipt: &ChildCleanup,
    allow_agents: bool,
) -> Result<(), ShepherdError> {
    if receipt.child.workspace_id.is_some()
        && receipt.child.workspace_id == std::env::var("HERDR_WORKSPACE_ID").ok()
    {
        return Err(denied());
    }
    if exists(&receipt.path)? {
        let current = preflight_integrated(
            h,
            ticket_wt,
            std::slice::from_ref(&receipt.child),
            allow_agents,
        )?;
        if current[0].path != receipt.path || current[0].tip != receipt.tip {
            return Err(denied());
        }
        verify_child_tip(ticket_wt, &receipt.child, &receipt.tip, true)?;
    } else {
        validate_removed(h, ticket_wt, receipt)?;
    }
    verify_integrated_tip(
        ticket_wt,
        &receipt.child,
        &receipt.tip,
        &git(ticket_wt, &["rev-parse", "HEAD"])?,
    )
}

fn check_persisted(
    store: &Store,
    expected: &Ticket,
    remaining: &[ChildWorktree],
    receipt: &ChildCleanup,
) -> Result<(), ShepherdError> {
    let data = store.read()?;
    let ticket = recorded(&data, expected, remaining)?;
    if !ticket.child_cleanup.contains(receipt) {
        return Err(denied());
    }
    validate_receipt(ticket, receipt, &data.agents)
}

fn preflight_agents(
    h: &dyn Herdr,
    store: &Store,
    receipt: &ChildCleanup,
) -> Result<(), ShepherdError> {
    let data = store.read()?;
    for info in h.agents()? {
        let related = receipt
            .child
            .workspace_id
            .as_ref()
            .is_some_and(|id| info.workspace_id.as_ref() == Some(id))
            || info.cwd.as_ref().is_some_and(|cwd| {
                cwd.canonicalize()
                    .unwrap_or_else(|_| cwd.clone())
                    .starts_with(&receipt.path)
            });
        if !related || matches!(info.agent_status.as_str(), "exited" | "stopped") {
            continue;
        }
        let Some(rec) = data.agents.iter().find(|rec| {
            rec.status == AgentStatus::Reported
                && rec.name.as_str() == info.name.as_deref().unwrap_or("")
                && rec.pane_id.as_deref() == Some(&info.pane_id)
                && rec.worktree.canonicalize().ok().as_ref() == Some(&receipt.path)
        }) else {
            return Err(ShepherdError::Usage(
                "child still has an unowned or active agent".into(),
            ));
        };
        if crate::flock::automatic::recipient(h, store, rec, true)?.is_none() {
            return Err(denied());
        }
    }
    Ok(())
}

/// Caller holds ticket admission/operation locks. Persist and validate the exact
/// owned resources before removing anything; ordinary retries consume receipts.
pub fn cleanup_integrated_recorded(
    h: &dyn Herdr,
    store: &Store,
    expected: &Ticket,
) -> Result<Vec<u32>, ShepherdError> {
    let ticket_wt = expected.worktree.as_deref().ok_or_else(denied)?;
    let _lock = integration_lock(ticket_wt)?;
    if git(ticket_wt, &["branch", "--show-current"])? != expected.branch {
        return Err(denied());
    }
    let data = store.read()?;
    let ticket = recorded(&data, expected, &expected.children)?;
    // Preflight the complete snapshot before any child is retired or removed.
    for receipt in &ticket.child_cleanup {
        validate_receipt(ticket, receipt, &data.agents)?;
    }
    let mut receipts = Vec::new();
    let mut seen = HashSet::new();
    for child in &expected.children {
        if child.unit == 0 || !seen.insert(child.unit) {
            return Err(denied());
        }
        let receipt = match ticket.child_cleanup.iter().find(|r| r.child == *child) {
            Some(receipt) => {
                validate_target(h, ticket_wt, receipt, true)?;
                receipt.clone()
            }
            None => {
                preflight_integrated(h, ticket_wt, std::slice::from_ref(child), true)?.remove(0)
            }
        };
        preflight_agents(h, store, &receipt)?;
        receipts.push(receipt);
    }
    store.update(|data| {
        let ticket = recorded(data, expected, &expected.children)?;
        if ticket.child_cleanup != expected.child_cleanup {
            return Err(denied());
        }
        let ticket = data
            .tickets
            .iter_mut()
            .find(|t| t.id == expected.id)
            .unwrap();
        for receipt in &receipts {
            if !ticket.child_cleanup.contains(receipt) {
                ticket.child_cleanup.push(receipt.clone());
            }
        }
        Ok(())
    })?;
    let mut remaining = expected.children.clone();
    let mut removed = Vec::new();
    for original in receipts {
        let data = store.read()?;
        let ticket = recorded(&data, expected, &remaining)?;
        let receipt = ticket
            .child_cleanup
            .iter()
            .find(|r| r.child == original.child)
            .ok_or_else(denied)?
            .clone();
        validate_receipt(ticket, &receipt, &data.agents)?;
        validate_target(h, ticket_wt, &receipt, true)?;
        for agent in data.agents.iter().filter(|a| {
            a.ticket == expected.id
                && a.status == AgentStatus::Reported
                && (a
                    .worktree
                    .canonicalize()
                    .ok()
                    .is_some_and(|p| p == receipt.path)
                    || receipt.retiring_agents.contains(a))
        }) {
            let fresh = store.read()?;
            let ticket = recorded(&fresh, expected, &remaining)?;
            let receipt = ticket
                .child_cleanup
                .iter()
                .find(|r| r.child == original.child)
                .ok_or_else(denied)?;
            validate_receipt(ticket, receipt, &fresh.agents)?;
            if !crate::flock::automatic::retire_for_cleanup(h, store, agent, receipt)? {
                return Err(ShepherdError::PolicyDenied(format!(
                    "{}: automatic retirement deferred; child resources kept",
                    agent.name
                )));
            }
        }
        // Retirement may have extended the receipt with validated tab identities.
        let data = store.read()?;
        let ticket = recorded(&data, expected, &remaining)?;
        let receipt = ticket
            .child_cleanup
            .iter()
            .find(|r| r.child == original.child)
            .ok_or_else(denied)?
            .clone();
        validate_receipt(ticket, &receipt, &data.agents)?;
        validate_target(h, ticket_wt, &receipt, false)?;
        check_persisted(store, expected, &remaining, &receipt)?;
        if exists(&receipt.path)? {
            if let Some(id) = &receipt.child.workspace_id {
                // Herdr may remove the checkout before closing the workspace.
                // Revalidate that handle below before any separate close.
                h.remove_worktree(id, false)?;
            } else {
                remove_child_checkout(h, ticket_wt, &receipt.child)?;
            }
        }
        // Removal may have succeeded before an adapter error; recovery enters here
        // only after the same missing-checkout proof on a later invocation.
        validate_removed(h, ticket_wt, &receipt)?;
        check_persisted(store, expected, &remaining, &receipt)?;
        if let Some(id) = &receipt.child.workspace_id
            && h.workspaces()?.iter().any(|ws| ws.workspace_id == *id)
        {
            h.close_workspace(id)?;
        }
        validate_removed(h, ticket_wt, &receipt)?;
        check_persisted(store, expected, &remaining, &receipt)?;
        verify_integrated_tip(
            ticket_wt,
            &receipt.child,
            &receipt.tip,
            &git(ticket_wt, &["rev-parse", "HEAD"])?,
        )?;
        if branch_exists(ticket_wt, &receipt.child.branch)? {
            git(
                ticket_wt,
                &[
                    "update-ref",
                    "-d",
                    &format!("refs/heads/{}", receipt.child.branch),
                    &receipt.tip,
                ],
            )?;
        }
        validate_removed(h, ticket_wt, &receipt)?;
        if branch_exists(ticket_wt, &receipt.child.branch)? {
            return Err(denied());
        }
        store.update(|data| {
            let ticket = recorded(data, expected, &remaining)?;
            if !ticket.child_cleanup.contains(&receipt) {
                return Err(denied());
            }
            let ticket = data
                .tickets
                .iter_mut()
                .find(|t| t.id == expected.id)
                .unwrap();
            ticket.children.retain(|c| c != &receipt.child);
            ticket.child_cleanup.retain(|r| r != &receipt);
            Ok(())
        })?;
        remaining.retain(|c| c != &receipt.child);
        removed.push(receipt.child.unit);
    }
    Ok(removed)
}
