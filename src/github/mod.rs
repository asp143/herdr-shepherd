use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{error::ShepherdError, gate::Gate};

pub mod codeowners;
#[cfg(any(test, debug_assertions, feature = "test-support"))]
pub mod fake;
pub mod gh;

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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PrComment {
    pub id: u64,
    pub author: String,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub body: String,
    pub review: bool,
}

pub trait PrBackend {
    fn requested_reviewers(
        &self,
        _gate: &Gate,
        _number: u64,
    ) -> Result<Vec<String>, ShepherdError> {
        Ok(vec![])
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
    fn reply(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<(), ShepherdError>;
}
