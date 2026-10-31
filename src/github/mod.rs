use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{error::ShepherdError, gate::Gate};

pub mod codeowners;
#[cfg(any(test, debug_assertions, feature = "test-support"))]
pub mod fake;
pub mod gh;
pub mod protection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Checks {
    Passing,
    Failing,
    Pending,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct PrStatus {
    pub number: u64,
    pub url: String,
    pub state: PrState,
    pub merged_at: Option<String>,
    pub base: String,
    pub head: String,
    #[serde(default)]
    pub head_oid: Option<String>,
    pub checks: Checks,
    pub review: ReviewDecision,
    pub comment_ids: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum CommentKind {
    Review,
    Issue,
    ReviewBody,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PrComment {
    #[serde(default)]
    pub kind: Option<CommentKind>,
    pub id: u64,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    pub review: bool,
}

impl PrComment {
    pub fn comment_kind(&self) -> CommentKind {
        self.kind.unwrap_or(if self.review {
            CommentKind::Review
        } else {
            CommentKind::Issue
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FailedCheck {
    pub name: String,
    pub url: String,
}

pub trait PrBackend {
    fn failed_checks(
        &self,
        _worktree: &Path,
        _number: u64,
    ) -> Result<Vec<FailedCheck>, ShepherdError> {
        Ok(vec![])
    }
    fn requested_reviewers(
        &self,
        _gate: &Gate,
        _number: u64,
    ) -> Result<Vec<String>, ShepherdError> {
        Ok(vec![])
    }

    fn branch_protected(&self, _worktree: &Path, _branch: &str) -> Result<bool, ShepherdError> {
        Err(ShepherdError::GithubUnavailable(
            "protection reads are unsupported".into(),
        ))
    }
    fn default_branch(&self, _worktree: &Path) -> Result<String, ShepherdError> {
        Err(ShepherdError::GithubUnavailable(
            "default branch reads are unsupported".into(),
        ))
    }

    fn comments(&self, _worktree: &Path, _number: u64) -> Result<Vec<PrComment>, ShepherdError> {
        Err(ShepherdError::GithubUnavailable(
            "comment reads are unsupported".into(),
        ))
    }
    fn author(&self, _worktree: &Path) -> Result<String, ShepherdError> {
        Err(ShepherdError::GithubUnavailable(
            "author reads are unsupported".into(),
        ))
    }

    fn view(&self, worktree: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError>;
    fn push(&self, gate: &Gate) -> Result<(), ShepherdError>;
    /// Backends must bind the explicit push source to the QA-tested commit.
    /// Legacy adapters fail closed until they implement that contract.
    fn push_expected(&self, _gate: &Gate, _expected: &str) -> Result<(), ShepherdError> {
        Err(ShepherdError::PolicyDenied(
            "backend cannot bind a revision push to its QA-tested commit".into(),
        ))
    }
    fn create(
        &self,
        gate: &Gate,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrStatus, ShepherdError>;
    fn add_reviewers(
        &self,
        gate: &Gate,
        number: u64,
        reviewers: &[String],
    ) -> Result<(), ShepherdError>;
    /// Return the REST id when the backend can identify the created comment.
    fn reply_id(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<Option<u64>, ShepherdError> {
        self.reply(gate, number, comment_id, body)?;
        Ok(None)
    }

    fn reply(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<(), ShepherdError>;
}
