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
    /// Preserve the queue entry's provenance when a proven rejection restores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_ping: Option<PendingPing>,
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
    pub pending_pings: Vec<PendingPing>,
    #[serde(default)]
    pub protection_cache: crate::github::protection::ProtectionCache,
}

/// Automatic notices expire after one day; explicit user requests never expire.
pub const MAX_NOTICE_AGE_SECONDS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PingKind {
    Request,
    Notice,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PendingPing {
    pub text: String,
    pub kind: PingKind,
    pub ticket: Option<String>,
    pub agent: Option<String>,
    pub created_at: u64,
}

impl PendingPing {
    pub fn request(text: &str) -> Self {
        Self {
            text: text.into(),
            kind: PingKind::Request,
            ticket: None,
            agent: None,
            created_at: queue_now(),
        }
    }

    pub fn notice(text: &str, ticket: Option<&str>, agent: Option<&str>) -> Self {
        Self {
            text: text.into(),
            kind: PingKind::Notice,
            ticket: ticket.map(str::to_owned),
            agent: agent.map(str::to_owned),
            created_at: queue_now(),
        }
    }

    fn legacy(text: String) -> Self {
        use std::sync::OnceLock;
        static SUPERVISION_NOTICE: OnceLock<regex::Regex> = OnceLock::new();
        static REPORTED_NOTICE: OnceLock<regex::Regex> = OnceLock::new();
        static TICKET_NOTICE: OnceLock<regex::Regex> = OnceLock::new();
        let supervision = SUPERVISION_NOTICE.get_or_init(|| {
            regex::Regex::new(concat!(
                r"(?s)^(?P<ticket>[A-Za-z0-9][A-Za-z0-9._/-]*|): ",
                r"(?P<agent>sh-[0-9a-f]{6}-(?:lead|[a-z0-9-]+-[1-9][0-9]*)) ",
                r"(?:blocked: .+|still blocked: .+|",
                r"prompt delivery uncertain; inspect submission before sending input|",
                r"idle without a report|stalled without progress|exited again after resume)$"
            ))
            .expect("fixed generated supervision notice regex")
        });
        let reported = REPORTED_NOTICE.get_or_init(|| regex::Regex::new(concat!(
            r"(?s)^(?P<ticket>[A-Za-z0-9][A-Za-z0-9._/-]*|): ",
            r"[a-z][a-z0-9_-]* (?P<agent>sh-[0-9a-f]{6}-(?:lead|[a-z0-9-]+-[1-9][0-9]*)) reported: .*$"
        )).expect("fixed generated report notice regex"));
        if let Some(captures) = supervision
            .captures(&text)
            .or_else(|| reported.captures(&text))
        {
            return Self::notice(&text, Some(&captures["ticket"]), Some(&captures["agent"]));
        }
        let ticket = TICKET_NOTICE.get_or_init(|| regex::Regex::new(concat!(
            r"^(?P<ticket>[A-Za-z0-9][A-Za-z0-9._/-]*): (?:",
            r"[a-z][a-z0-9_-]* launch_failed; role re-queued for agent spawn|",
            r"[1-9][0-9]* new PR comment\(s\), run ticket comments (?P<comments_ticket>[A-Za-z0-9][A-Za-z0-9._/-]*) --new|",
            r"CI failed; run ticket dispatch to start the fix and complete reviewer and QA|",
            r"child integration conflict; both ranges retained|",
            r"revision blocked \(conflict\): .+|",
            r"run ticket ack (?P<ack_ticket>[A-Za-z0-9][A-Za-z0-9._/-]*) --comment [0-9]+ --body-file <path>; explain the change and include commit [0-9a-f]+)$"
        )).expect("fixed generated ticket notice regex"));
        if let Some(captures) = ticket.captures(&text)
            && ["comments_ticket", "ack_ticket"].iter().all(|name| {
                captures
                    .name(name)
                    .is_none_or(|repeated| repeated.as_str() == &captures["ticket"])
            })
        {
            return Self::notice(&text, Some(&captures["ticket"]), None);
        }
        Self::request(&text)
    }
}

impl<'de> Deserialize<'de> for PendingPing {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Legacy(String),
            Structured {
                text: String,
                kind: PingKind,
                ticket: Option<String>,
                agent: Option<String>,
                created_at: u64,
            },
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Legacy(text) => Self::legacy(text),
            Wire::Structured {
                text,
                kind,
                ticket,
                agent,
                created_at,
            } => Self {
                text,
                kind,
                ticket,
                agent,
                created_at,
            },
        })
    }
}

impl From<String> for PendingPing {
    fn from(text: String) -> Self {
        Self::legacy(text)
    }
}
impl From<&str> for PendingPing {
    fn from(text: &str) -> Self {
        Self::legacy(text.into())
    }
}
impl std::ops::Deref for PendingPing {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}
impl AsRef<str> for PendingPing {
    fn as_ref(&self) -> &str {
        &self.text
    }
}
impl PartialEq<str> for PendingPing {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}
impl PartialEq<&str> for PendingPing {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}
impl PartialEq<String> for PendingPing {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}
impl std::fmt::Display for PendingPing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.text.fmt(f)
    }
}

fn generated_lead_name(name: &str) -> bool {
    name.strip_prefix("sh-")
        .and_then(|name| name.strip_suffix("-lead"))
        .is_some_and(|hash| {
            hash.len() == 6
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn queue_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl StoreData {
    pub const MAX_PENDING_PINGS: usize = 256;

    fn pending_lead_requests(&self) -> usize {
        self.pending_pings.len().saturating_add(
            self.agents
                .iter()
                .filter_map(AgentRecord::prompt_claim)
                .filter(|claim| claim.kind == "notification")
                .count(),
        )
    }

    /// Explicit requests retain their complete text and intentional duplicates.
    pub fn enqueue_lead_request(&mut self, text: &str) -> Result<(), crate::error::ShepherdError> {
        if self.pending_lead_requests() >= Self::MAX_PENDING_PINGS {
            return Err(crate::error::ShepherdError::Usage(
                "lead request queue is full; retry after pending requests are delivered".into(),
            ));
        }
        self.pending_pings.push(PendingPing::request(text));
        Ok(())
    }

    pub fn ping_is_stale(&self, ping: &PendingPing, now: u64) -> bool {
        if ping.kind == PingKind::Request {
            return false;
        }
        if now.saturating_sub(ping.created_at) > MAX_NOTICE_AGE_SECONDS {
            return true;
        }
        if ping.ticket.as_deref() == Some("") {
            return true;
        }
        if let Some(name) = ping.agent.as_deref()
            && (self
                .lead
                .as_ref()
                .is_some_and(|lead| lead.agent_name == name)
                || self
                    .agents
                    .iter()
                    .any(|agent| agent.name == name && agent.role == "lead")
                || generated_lead_name(name))
        {
            return true;
        }
        let ticket = ping
            .ticket
            .as_ref()
            .and_then(|id| self.tickets.iter().find(|ticket| ticket.id == *id));
        if ping.ticket.is_some()
            && ticket.is_none_or(|ticket| {
                matches!(
                    ticket.state,
                    TicketState::Closed | TicketState::CleanedUp | TicketState::Merged
                )
            })
        {
            return true;
        }
        let ticket_active = ticket.is_some_and(|ticket| {
            !matches!(
                ticket.state,
                TicketState::Closed
                    | TicketState::CleanedUp
                    | TicketState::Merged
                    | TicketState::Failed
            )
        });
        if let Some(name) = ping.agent.as_deref()
            && !ticket_active
            && self
                .agents
                .iter()
                .find(|agent| agent.name == name)
                .is_none_or(|agent| {
                    matches!(
                        agent.status,
                        AgentStatus::Retired | AgentStatus::LaunchFailed
                    )
                })
        {
            return true;
        }
        false
    }

    fn trim_automatic(&mut self, limit: usize) {
        while self.pending_lead_requests() > limit {
            let oldest = self
                .pending_pings
                .iter()
                .enumerate()
                .filter(|(_, ping)| ping.kind == PingKind::Notice)
                .min_by_key(|(index, ping)| (ping.created_at, *index))
                .map(|(index, _)| index);
            let Some(index) = oldest else {
                break;
            };
            self.pending_pings.remove(index);
        }
    }

    /// Runs under Store::update. Never touches requests or wakes the recipient.
    pub fn heal_pending_pings(&mut self, now: u64) -> usize {
        let before = self.pending_pings.len();
        self.pending_pings = std::mem::take(&mut self.pending_pings)
            .into_iter()
            .filter(|ping| !self.ping_is_stale(ping, now))
            .collect();
        self.trim_automatic(Self::MAX_PENDING_PINGS);
        before - self.pending_pings.len()
    }

    /// Compatibility helper for automatic callers without explicit identity.
    pub fn enqueue_lead_ping(&mut self, text: &str) {
        let mut ping = PendingPing::legacy(text.into());
        ping.kind = PingKind::Notice;
        self.enqueue_ping(ping, true);
    }

    pub fn enqueue_lead_notice(&mut self, text: &str, ticket: Option<&str>, agent: Option<&str>) {
        self.enqueue_ping(PendingPing::notice(text, ticket, agent), true);
    }

    /// The observation episode owns dedupe, so identical later episodes survive.
    pub fn enqueue_lead_episode_notice(
        &mut self,
        text: &str,
        ticket: Option<&str>,
        agent: Option<&str>,
    ) {
        self.enqueue_ping(PendingPing::notice(text, ticket, agent), false);
    }

    fn enqueue_ping(&mut self, ping: PendingPing, dedupe: bool) {
        let before = self.pending_pings.len();
        self.heal_pending_pings(queue_now());
        if !self.ping_is_stale(&ping, queue_now())
            && (!dedupe
                || (!self
                    .pending_pings
                    .iter()
                    .any(|pending| pending.text == ping.text)
                    && !self
                        .agents
                        .iter()
                        .filter_map(AgentRecord::prompt_claim)
                        .any(|claim| claim.kind == "notification" && claim.text == ping.text)))
        {
            self.trim_automatic(Self::MAX_PENDING_PINGS.saturating_sub(1));
            let removed = before - self.pending_pings.len();
            if removed > 0 {
                eprintln!("shepherd: healed {removed} queued notice(s)");
            }
            if self.pending_lead_requests() < Self::MAX_PENDING_PINGS {
                self.pending_pings.push(ping);
            }
        } else {
            let removed = before - self.pending_pings.len();
            if removed > 0 {
                eprintln!("shepherd: healed {removed} queued notice(s)");
            }
        }
    }
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
