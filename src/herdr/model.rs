use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    pub worktree: Option<WorktreeInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub repo_key: String,
    pub repo_root: Option<PathBuf>,
    pub checkout_path: Option<PathBuf>,
    pub is_linked_worktree: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_string")]
    pub agent: String,
    #[serde(default, deserialize_with = "null_string")]
    pub agent_status: String,
    pub cwd: Option<PathBuf>,
    #[serde(default, deserialize_with = "null_string")]
    pub pane_id: String,
    pub workspace_id: Option<String>,
}

fn null_string<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

pub fn parse_snapshot(json: &str) -> anyhow::Result<Vec<Workspace>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let workspaces = value
        .pointer("/result/snapshot/workspaces")
        .context("herdr snapshot response has no workspaces")?;
    serde_json::from_value(workspaces.clone()).context("invalid herdr workspaces")
}

pub fn parse_agent_list(json: &str) -> anyhow::Result<Vec<AgentInfo>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    let agents = value
        .pointer("/result/agents")
        .context("herdr agent list response has no agents")?;
    serde_json::from_value(agents.clone()).context("invalid herdr agents")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_snapshot_keeps_order_and_worktree() {
        let workspaces =
            parse_snapshot(include_str!("../../tests/fixtures/snapshot.json")).unwrap();
        assert_eq!(
            workspaces
                .iter()
                .map(|w| w.workspace_id.as_str())
                .collect::<Vec<_>>(),
            [
                "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8", "w9", "w10", "w11", "w12", "w13"
            ]
        );
        assert!(
            workspaces
                .iter()
                .any(|w| w.worktree.as_ref().is_some_and(|w| w.is_linked_worktree))
        );
    }

    #[test]
    fn parse_snapshot_tolerates_missing_worktree() {
        let workspaces = parse_snapshot(
            r#"{"result":{"snapshot":{"workspaces":[{"workspace_id":"w1","label":"scratch"}]}}}"#,
        )
        .unwrap();
        assert_eq!(workspaces.len(), 1);
        assert!(workspaces[0].worktree.is_none());
    }

    #[test]
    fn parse_agent_list_tolerates_null_fields() {
        let agents = parse_agent_list(r#"{"result":{"agents":[{"name":null,"agent":null,"agent_status":null,"cwd":null,"pane_id":null,"workspace_id":null},{}]}}"#).unwrap();
        assert_eq!(agents.len(), 2);
        for agent in agents {
            assert!(agent.agent.is_empty());
            assert!(agent.agent_status.is_empty());
            assert!(agent.pane_id.is_empty());
            assert_eq!(agent.name, None);
            assert_eq!(agent.cwd, None);
            assert_eq!(agent.workspace_id, None);
        }
    }

    #[test]
    fn parse_agent_list_handles_null_name() {
        let agents =
            parse_agent_list(include_str!("../../tests/fixtures/agent_list.json")).unwrap();
        assert!(!agents.is_empty());
        assert_eq!(agents[0].name, None);
    }
}
