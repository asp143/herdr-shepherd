use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{error::ShepherdError, gate::Gate};

pub mod codeowners;
#[cfg(test)]
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

pub trait PrBackend {
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
