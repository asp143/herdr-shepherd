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
    pub pr_reviewers_pending: Vec<String>,
    pub children: Vec<ChildWorktree>,
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
pub struct StoreData {
    pub schema_version: u32,
    pub project_root: PathBuf,
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
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommentRecord {
    pub id: u64,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    #[serde(default)]
    pub triaged: bool,
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub ack_sha: Option<String>,
    #[serde(default)]
    pub acknowledged: bool,
    #[serde(default = "review_comment")]
    pub review: bool,
}

fn review_comment() -> bool {
    true
}
