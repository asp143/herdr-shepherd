use std::{cell::RefCell, collections::VecDeque, path::Path};

use super::{PrBackend, PrStatus};
use crate::{error::ShepherdError, gate::Gate};

/// Deterministic backend: queued observations never contact GitHub.
pub struct FakeBackend {
    pub views: RefCell<VecDeque<Result<Option<PrStatus>, ShepherdError>>>,
    pub calls: RefCell<Vec<String>>,
}

impl FakeBackend {
    pub fn new(views: Vec<Result<Option<PrStatus>, ShepherdError>>) -> Self {
        Self {
            views: RefCell::new(views.into()),
            calls: RefCell::new(vec![]),
        }
    }
}

impl PrBackend for FakeBackend {
    fn view(&self, worktree: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("view {} {branch}", worktree.display()));
        self.views.borrow_mut().pop_front().unwrap_or(Ok(None))
    }

    fn push(&self, gate: &Gate) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("push {}", gate.ticket_branch));
        Ok(())
    }

    fn create(
        &self,
        gate: &Gate,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrStatus, ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("create {base} {title} {body}"));
        self.view(&gate.worktree, &gate.ticket_branch)?
            .ok_or_else(|| ShepherdError::Runtime(anyhow::anyhow!("missing fake create status")))
    }

    fn add_reviewers(
        &self,
        _: &Gate,
        number: u64,
        reviewers: &[String],
    ) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("reviewers {number} {reviewers:?}"));
        Ok(())
    }

    fn reply(
        &self,
        _: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<(), ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("reply {number} {comment_id:?} {body}"));
        Ok(())
    }
}
