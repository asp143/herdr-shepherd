use std::{
    fs,
    path::{Path, PathBuf},
    thread,
};

use serde::Serialize;

use crate::{
    herdr::{AgentInfo, Workspace},
    probe::Probe,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Kind {
    Merged,
    Closed,
    Orphan,
    Stray,
    #[serde(rename = "NO-PR")]
    NoPr,
    Open,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
            Self::Orphan => "ORPHAN",
            Self::Stray => "STRAY",
            Self::NoPr => "NO-PR",
            Self::Open => "OPEN",
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub repo: String,
    pub entry: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub branch: Option<String>,
    pub workspace_id: Option<String>,
    pub label: Option<String>,
    pub flags: Vec<String>,
    pub size: Option<String>,
}

pub fn scan(
    root: &Path,
    ws: &[Workspace],
    agents: &[AgentInfo],
    probe: &dyn Probe,
    with_size: bool,
) -> Vec<Entry> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut paths = Vec::new();
    if let Ok(repos) = fs::read_dir(&root) {
        for repo in repos.flatten() {
            if !repo.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            if let Ok(entries) = fs::read_dir(repo.path()) {
                paths.extend(entries.flatten().map(|entry| entry.path()));
            }
        }
    }
    if paths.is_empty() {
        return Vec::new();
    }
    paths.sort();
    let mut rows = thread::scope(|scope| {
        let handles = paths
            .chunks(paths.len().div_ceil(8))
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|p| inspect(p, ws, agents, probe, with_size))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().ok())
            .flatten()
            .collect::<Vec<_>>()
    });
    rows.sort_by(|a, b| (&a.kind, &a.repo, &a.entry).cmp(&(&b.kind, &b.repo, &b.entry)));
    rows
}

fn inspect(
    p: &Path,
    ws: &[Workspace],
    agents: &[AgentInfo],
    probe: &dyn Probe,
    with_size: bool,
) -> Option<Entry> {
    let metadata = fs::symlink_metadata(p).ok()?;
    let entry = p.file_name()?.to_string_lossy().into_owned();
    let repo = p.parent()?.file_name()?.to_string_lossy().into_owned();
    let workspace = ws
        .iter()
        .find(|w| w.worktree.as_ref().and_then(|w| w.checkout_path.as_deref()) == Some(p));
    let mut flags = Vec::new();
    let mut branch = None;
    let kind = if !metadata.is_dir() {
        Kind::Stray
    } else if !fs::symlink_metadata(p.join(".git"))
        .is_ok_and(|metadata| metadata.is_file() || metadata.is_dir())
    {
        // Git follows a .git symlink, so only probe ordinary files/directories.
        Kind::Orphan
    } else {
        branch = probe.branch(p);
        if branch
            .as_ref()
            .is_some_and(|b| b.replace('/', "-") != entry)
        {
            flags.push("BRANCH-SWITCHED".into());
        }
        if probe.dirty(p) {
            flags.push("DIRTY".into());
        }
        match branch
            .as_ref()
            .and_then(|branch| probe.pr_state(p, branch))
            .as_deref()
        {
            Some("MERGED") => Kind::Merged,
            Some("CLOSED") => Kind::Closed,
            Some("OPEN") => Kind::Open,
            _ => Kind::NoPr,
        }
    };
    let count = agents
        .iter()
        .filter(|a| a.cwd.as_ref().is_some_and(|cwd| cwd.starts_with(p)))
        .count();
    if count > 0 {
        flags.push(format!("AGENTS={count}"));
    }
    Some(Entry {
        repo,
        entry,
        path: p.to_path_buf(),
        kind,
        branch,
        workspace_id: workspace.map(|w| w.workspace_id.clone()),
        label: workspace.map(|w| w.label.clone()),
        flags,
        size: if with_size && !metadata.file_type().is_symlink() {
            probe.size(p)
        } else {
            None
        },
    })
}

pub fn human_report(entries: &[Entry], show_all: bool) -> String {
    use std::fmt::Write;
    let mut report = String::new();
    let mut hidden = 0;
    for row in entries {
        if !show_all && row.kind == Kind::Open {
            hidden += 1;
            continue;
        }
        let kind = row.kind.to_string();
        let _ = write!(
            report,
            "{kind:<7} {:>5}  {}/{}",
            row.size.as_deref().unwrap_or(""),
            row.repo,
            row.entry
        );
        if let Some(branch) = &row.branch {
            let _ = write!(report, " [{branch}]");
        }
        if let Some(id) = &row.workspace_id {
            let _ = write!(report, " ws={id}");
        }
        for flag in &row.flags {
            let _ = write!(report, " {flag}");
        }
        report.push('\n');
    }
    let _ = write!(
        report,
        "\n{} entries, {hidden} OPEN hidden (--all to show). Report only; nothing deleted.",
        entries.len()
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct FakeProbe;

    impl Probe for FakeProbe {
        fn branch(&self, _: &Path) -> Option<String> {
            Some("fix/a".into())
        }
        fn dirty(&self, _: &Path) -> bool {
            true
        }
        fn pr_state(&self, p: &Path, _: &str) -> Option<String> {
            match p.file_name()?.to_str()? {
                "feat-a" | "merged" => Some("MERGED".into()),
                "closed" => Some("CLOSED".into()),
                "open" => Some("OPEN".into()),
                _ => None,
            }
        }
        fn size(&self, _: &Path) -> Option<String> {
            Some("4K".into())
        }
    }

    fn entry(root: &Path, name: &str, git: bool) -> PathBuf {
        let p = root.join("example").join(name);
        fs::create_dir_all(&p).unwrap();
        if git {
            fs::write(p.join(".git"), "gitdir: /missing\n").unwrap();
        }
        p
    }

    #[test]
    fn classifies_stray_and_orphan() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "orphan", false);
        fs::write(root.path().join("example/stray.txt"), "ordinary fixture").unwrap();
        let rows = scan(root.path(), &[], &[], &FakeProbe, false);
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            [Kind::Orphan, Kind::Stray]
        );
        assert!(rows.iter().all(|r| r.branch.is_none() && r.size.is_none()));
    }

    #[test]
    fn merged_with_flags() {
        let root = tempfile::tempdir().unwrap();
        let p = entry(root.path(), "feat-a", true);
        let agents = [AgentInfo {
            name: None,
            agent: "codex".into(),
            agent_status: "working".into(),
            cwd: Some(p.join("src")),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        }];
        let ws = [Workspace {
            workspace_id: "w1".into(),
            label: "task".into(),
            worktree: Some(crate::herdr::WorktreeInfo {
                repo_key: "example".into(),
                repo_root: None,
                checkout_path: Some(p),
                is_linked_worktree: true,
            }),
        }];
        let rows = scan(root.path(), &ws, &agents, &FakeProbe, true);
        assert_eq!(rows[0].kind, Kind::Merged);
        for flag in ["DIRTY", "AGENTS=1", "BRANCH-SWITCHED"] {
            assert!(rows[0].flags.iter().any(|f| f == flag));
        }
        assert_eq!(rows[0].workspace_id.as_deref(), Some("w1"));
        assert_eq!(rows[0].label.as_deref(), Some("task"));
        assert_eq!(rows[0].size.as_deref(), Some("4K"));
    }

    #[test]
    fn no_gh_falls_back_to_no_pr() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "no-pr", true);
        assert_eq!(
            scan(root.path(), &[], &[], &FakeProbe, false)[0].kind,
            Kind::NoPr
        );
    }

    #[test]
    fn sort_order() {
        let root = tempfile::tempdir().unwrap();
        for name in ["open", "no-pr", "closed", "merged"] {
            entry(root.path(), name, true);
        }
        entry(root.path(), "orphan", false);
        fs::write(root.path().join("example/stray.txt"), "fixture").unwrap();
        let rows = scan(root.path(), &[], &[], &FakeProbe, false);
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            [
                Kind::Merged,
                Kind::Closed,
                Kind::Orphan,
                Kind::Stray,
                Kind::NoPr,
                Kind::Open
            ]
        );
        assert_eq!(
            serde_json::to_value(rows.iter().map(|r| r.kind).collect::<Vec<_>>()).unwrap(),
            serde_json::json!(["MERGED", "CLOSED", "ORPHAN", "STRAY", "NO-PR", "OPEN"])
        );
    }

    #[test]
    fn symlinks_do_not_probe_outside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("example")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("example/link")).unwrap();
        let rows = scan(root.path(), &[], &[], &FakeProbe, false);
        assert_eq!(rows[0].kind, Kind::Stray);
        assert!(rows[0].branch.is_none());
    }

    #[test]
    fn git_symlink_does_not_probe_outside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let p = entry(root.path(), "symlink-git", false);
        std::os::unix::fs::symlink(outside.path(), p.join(".git")).unwrap();
        let rows = scan(root.path(), &[], &[], &FakeProbe, false);
        assert_eq!(rows[0].kind, Kind::Orphan);
        assert!(rows[0].branch.is_none());
        assert!(rows[0].flags.is_empty());
    }

    #[test]
    fn relative_root_matches_absolute_workspace_and_agent_paths() {
        let root = tempfile::tempdir_in(".").unwrap();
        let p = entry(root.path(), "feat-a", true).canonicalize().unwrap();
        let ws = [Workspace {
            workspace_id: "w1".into(),
            label: "task".into(),
            worktree: Some(crate::herdr::WorktreeInfo {
                repo_key: "example".into(),
                repo_root: None,
                checkout_path: Some(p.clone()),
                is_linked_worktree: true,
            }),
        }];
        let agents = [AgentInfo {
            name: None,
            agent: "codex".into(),
            agent_status: "working".into(),
            cwd: Some(p.join("src")),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        }];
        let cwd = std::env::current_dir().unwrap();
        let relative = root.path().strip_prefix(&cwd).unwrap();
        let rows = scan(relative, &ws, &agents, &FakeProbe, false);
        assert_eq!(rows[0].workspace_id.as_deref(), Some("w1"));
        assert!(rows[0].flags.contains(&"AGENTS=1".to_owned()));
    }

    #[test]
    fn orphan_reports_agents_inside_it() {
        let root = tempfile::tempdir().unwrap();
        let p = entry(root.path(), "orphan", false);
        let agents = [AgentInfo {
            name: None,
            agent: "codex".into(),
            agent_status: "working".into(),
            cwd: Some(p.join("src")),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        }];
        let rows = scan(root.path(), &[], &agents, &FakeProbe, false);
        assert_eq!(rows[0].kind, Kind::Orphan);
        assert!(rows[0].flags.contains(&"AGENTS=1".to_owned()));
    }

    #[test]
    fn probes_in_parallel_with_at_most_eight_workers() {
        use std::{
            sync::atomic::{AtomicUsize, Ordering},
            time::Duration,
        };
        struct ConcurrentProbe {
            active: AtomicUsize,
            maximum: AtomicUsize,
        }
        impl Probe for ConcurrentProbe {
            fn branch(&self, _: &Path) -> Option<String> {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.maximum.fetch_max(active, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(10));
                self.active.fetch_sub(1, Ordering::SeqCst);
                Some("feat/example".into())
            }
            fn dirty(&self, _: &Path) -> bool {
                false
            }
            fn pr_state(&self, _: &Path, _: &str) -> Option<String> {
                None
            }
            fn size(&self, _: &Path) -> Option<String> {
                None
            }
        }
        let root = tempfile::tempdir().unwrap();
        for i in 0..16 {
            entry(root.path(), &format!("task-{i}"), true);
        }
        let probe = ConcurrentProbe {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        };
        assert_eq!(scan(root.path(), &[], &[], &probe, false).len(), 16);
        assert!((2..=8).contains(&probe.maximum.load(Ordering::SeqCst)));
    }
}
