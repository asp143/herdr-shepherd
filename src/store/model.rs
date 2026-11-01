use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TicketState {
    #[default]
    Queued,
    Planning,
    Researching,
    Building,
    Reviewing,
    Qa,
    ReadyForPr,
    CiWait,
    PrOpen,
    Revising,
    Merged,
    CleanedUp,
    Closed,
    Failed,
    Blocked,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    LoopCap,
    Policy,
    Escalated,
    Conflict,
    LaunchFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Transition {
    pub from: TicketState,
    pub to: TicketState,
    pub reason: Option<BlockedReason>,
    pub at: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub acceptance: Vec<String>,
    pub branch: String,
    pub base: String,
    pub state: TicketState,
    pub blocked_reason: Option<BlockedReason>,
    pub worktree: Option<PathBuf>,
    pub workspace_id: Option<String>,
    pub plan: Option<String>,
    pub research: Vec<String>,
    pub revision_round: u32,
    pub findings: Vec<Finding>,
    pub pr_number: Option<u64>,
    #[serde(default)]
    pub ci_failure_task: Option<String>,
    #[serde(default)]
    pub ci_none_since: Option<String>,
    #[serde(default)]
    pub ci_head_sha: Option<String>,
    #[serde(default)]
    pub qa_commit_sha: Option<String>,
    #[serde(default)]
    pub pr_reviewers_pending: Vec<String>,
    #[serde(default)]
    pub posted_replies: Vec<PostedReply>,
    pub children: Vec<ChildWorktree>,
    #[serde(default)]
    pub integration_ranges: Vec<String>,
    pub history: Vec<Transition>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub comments: Vec<CommentRecord>,
    #[serde(default)]
    pub revision_task: Option<String>,
    #[serde(default)]
    pub revision_run: Option<crate::ticket::revise::RevisionRun>,
    #[serde(default)]
    pub protection_warnings: Vec<crate::github::protection::ProtectionWarning>,
    #[serde(default)]
    pub merged_while_blocked_notified: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PostedReply {
    pub id: Option<u64>,
    pub author: String,
    pub body: String,
    pub review: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChildWorktree {
    pub unit: u32,
    pub branch: String,
    pub path: PathBuf,
    pub workspace_id: String,
    pub base_sha: String,
    #[serde(default)]
    pub integrated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentRecord {
    pub name: String,
    pub ticket: String,
    pub role: String,
    pub runner: String,
    pub status: AgentStatus,
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub worktree: PathBuf,
    pub task: String,
    pub report: Option<Report>,
    pub observed: Option<String>,
    pub last_progress_at: String,
    pub nudges: u32,
    pub restarts: u32,
    #[serde(default)]
    pub prompt_pending: bool,
}

/// Persisted before automatic input. Follow-up acceptance cannot be inferred
/// from Working because that may describe an earlier turn in the same session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptClaim {
    pub kind: String,
    pub text: String,
}

impl AgentRecord {
    pub fn prompt_claim(&self) -> Option<PromptClaim> {
        self.observed
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .and_then(|value| serde_json::from_value(value.get("prompt_claim")?.clone()).ok())
    }

    pub(crate) fn set_prompt_claim(&mut self, claim: Option<PromptClaim>) {
        let mut observation = self.observation();
        observation["prompt_claim"] = serde_json::to_value(claim).expect("claim serializes");
        self.observed = Some(observation.to_string());
    }

    fn observation(&self) -> serde_json::Value {
        self.observed
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}))
    }

    pub fn prompt_delivery_uncertain(&self) -> bool {
        self.observed.as_deref().is_some_and(|text| {
            // Older launchers wrote this literal before supervision observations.
            text == "prompt_delivery_uncertain"
                || serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| {
                    value
                        .get("prompt_delivery_uncertain")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                })
        })
    }

    pub fn set_prompt_delivery_uncertain(&mut self, uncertain: bool) {
        let mut observation = self.observation();
        observation["prompt_delivery_uncertain"] = uncertain.into();
        self.observed = Some(observation.to_string());
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Working,
    Blocked,
    Reported,
    Retired,
    LaunchFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Report {
    pub status: String,
    pub summary: String,
    pub tests: Vec<String>,
    pub changed_paths: Vec<String>,
    pub blockers: Vec<String>,
    pub findings: Vec<Finding>,
    pub commit_sha: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
    pub location: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Mid,
    Low,
    Wish,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProjectTrust {
    pub project_root: PathBuf,
    pub common_git_dir: PathBuf,
    pub origin_url: String,
    pub push_url: String,
    pub default_branch: String,
    pub config_commit: Option<String>,
    pub config_blob: Option<String>,
    pub config_text: Option<String>,
    pub trusted_at: String,
    pub global_config: Option<PathBuf>,
    #[serde(default = "default_object_format")]
    pub object_format: String,
}
fn default_object_format() -> String {
    "sha1".into()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoreData {
    pub schema_version: u32,
    pub project_root: PathBuf,
    #[serde(default)]
    pub trust: Option<ProjectTrust>,
    pub lead: Option<LeadRecord>,
    pub tickets: Vec<Ticket>,
    pub agents: Vec<AgentRecord>,
    #[serde(default)]
    pub pending_pings: Vec<String>,
    #[serde(default)]
    pub protection_cache: crate::github::protection::ProtectionCache,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LeadRecord {
    pub agent_name: String,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub started_at: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommentRecord {
    #[serde(default)]
    pub kind: Option<crate::github::CommentKind>,
    pub id: u64,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    #[serde(default)]
    pub triaged: bool,
    #[serde(default)]
    pub pinged: bool,
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub ack_sha: Option<String>,
    #[serde(default)]
    pub acknowledged: bool,
    #[serde(default = "review_comment")]
    pub review: bool,
}

impl CommentRecord {
    pub fn comment_kind(&self) -> crate::github::CommentKind {
        self.kind.unwrap_or(if self.review {
            crate::github::CommentKind::Review
        } else {
            crate::github::CommentKind::Issue
        })
    }
}

fn review_comment() -> bool {
    true
}
