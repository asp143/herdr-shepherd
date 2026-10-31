use std::{cell::RefCell, collections::VecDeque, path::Path};

use super::{PrBackend, PrComment, PrStatus};
use crate::{error::ShepherdError, gate::Gate};

/// Deterministic backend: queued observations never contact GitHub.
pub struct FakeBackend {
    pub views: RefCell<VecDeque<Result<Option<PrStatus>, ShepherdError>>>,
    pub comments: RefCell<Vec<PrComment>>,
    pub author: RefCell<String>,
    pub protected: RefCell<std::collections::HashMap<String, Result<bool, String>>>,
    pub default_branch: RefCell<Result<String, String>>,
    pub calls: RefCell<Vec<String>>,
}

impl FakeBackend {
    pub fn new(views: Vec<Result<Option<PrStatus>, ShepherdError>>) -> Self {
        Self {
            views: RefCell::new(views.into()),
            comments: RefCell::new(vec![]),
            author: RefCell::new("fixture-author".into()),
            protected: RefCell::new(Default::default()),
            default_branch: RefCell::new(Err("default branch reads are unsupported".into())),
            calls: RefCell::new(vec![]),
        }
    }
}

impl PrBackend for FakeBackend {
    fn branch_protected(&self, _: &Path, branch: &str) -> Result<bool, ShepherdError> {
        self.calls
            .borrow_mut()
            .push(format!("branch_protected {branch}"));
        self.protected
            .borrow()
            .get(branch)
            .cloned()
            .unwrap_or_else(|| Err("protection reads are unsupported".into()))
            .map_err(ShepherdError::GithubUnavailable)
    }
    fn default_branch(&self, _: &Path) -> Result<String, ShepherdError> {
        self.calls.borrow_mut().push("default_branch".into());
        self.default_branch
            .borrow()
            .clone()
            .map_err(ShepherdError::GithubUnavailable)
    }

    fn comments(&self, _: &Path, number: u64) -> Result<Vec<PrComment>, ShepherdError> {
        self.calls.borrow_mut().push(format!("comments {number}"));
        Ok(self.comments.borrow().clone())
    }
    fn author(&self, _: &Path) -> Result<String, ShepherdError> {
        self.calls.borrow_mut().push("author".into());
        Ok(self.author.borrow().clone())
    }

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

    fn reply_id(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<Option<u64>, ShepherdError> {
        self.reply(gate, number, comment_id, body)?;
        let id = self
            .comments
            .borrow()
            .iter()
            .map(|comment| comment.id)
            .max()
            .unwrap_or(0)
            .max(1_000_000)
            + 1;
        self.comments.borrow_mut().push(PrComment {
            kind: None,
            id,
            author: self.author.borrow().clone(),
            path: None,
            line: None,
            body: body.into(),
            review: comment_id.is_some(),
        });
        Ok(Some(id))
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

/// Debug-only durable fixture backend shared by separate CLI invocations.
pub struct DiskBackend {
    path: std::path::PathBuf,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct DiskState {
    #[serde(default)]
    status: Option<PrStatus>,
    #[serde(default)]
    comments: Vec<PrComment>,
    #[serde(default)]
    failed_checks: Vec<super::FailedCheck>,
    #[serde(default)]
    author: String,
    #[serde(default)]
    protected: std::collections::HashMap<String, bool>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    calls: Vec<String>,
}

impl DiskBackend {
    pub fn open(directory: &Path) -> Result<Self, ShepherdError> {
        let path = directory.join("github.json");
        let _: DiskState =
            serde_json::from_slice(&std::fs::read(&path).map_err(anyhow::Error::from)?)
                .map_err(anyhow::Error::from)?;
        Ok(Self { path })
    }

    fn call<T>(
        &self,
        operation: String,
        f: impl FnOnce(&DiskState) -> Result<T, ShepherdError>,
    ) -> Result<T, ShepherdError> {
        let mut state: DiskState =
            serde_json::from_slice(&std::fs::read(&self.path).map_err(anyhow::Error::from)?)
                .map_err(anyhow::Error::from)?;
        state.calls.push(operation);
        std::fs::write(
            &self.path,
            serde_json::to_vec_pretty(&state).map_err(anyhow::Error::from)?,
        )
        .map_err(anyhow::Error::from)?;
        f(&state)
    }
}

impl PrBackend for DiskBackend {
    fn branch_protected(&self, _: &Path, branch: &str) -> Result<bool, ShepherdError> {
        self.call(format!("branch_protected {branch}"), |state| {
            state.protected.get(branch).copied().ok_or_else(|| {
                ShepherdError::GithubUnavailable("protection reads are unsupported".into())
            })
        })
    }
    fn default_branch(&self, _: &Path) -> Result<String, ShepherdError> {
        self.call("default_branch".into(), |state| {
            state.default_branch.clone().ok_or_else(|| {
                ShepherdError::GithubUnavailable("default branch reads are unsupported".into())
            })
        })
    }

    fn view(&self, _: &Path, branch: &str) -> Result<Option<PrStatus>, ShepherdError> {
        self.call(format!("view {branch}"), |s| Ok(s.status.clone()))
    }
    fn comments(&self, _: &Path, number: u64) -> Result<Vec<PrComment>, ShepherdError> {
        self.call(format!("comments {number}"), |s| Ok(s.comments.clone()))
    }
    fn failed_checks(
        &self,
        _: &Path,
        number: u64,
    ) -> Result<Vec<super::FailedCheck>, ShepherdError> {
        self.call(format!("failed_checks {number}"), |state| {
            Ok(state.failed_checks.clone())
        })
    }
    fn author(&self, _: &Path) -> Result<String, ShepherdError> {
        self.call("author".into(), |s| Ok(s.author.clone()))
    }
    fn push(&self, gate: &Gate) -> Result<(), ShepherdError> {
        crate::gate::check_push(
            &crate::gate::parse_push_args(&["origin", &gate.ticket_branch]),
            &gate.ticket_branch,
            &gate.policy,
        )?;
        self.call(format!("push {}", gate.ticket_branch), |_| Ok(()))
    }
    fn create(
        &self,
        _: &Gate,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PrStatus, ShepherdError> {
        self.call(format!("create {base} {title} {body}"), |s| {
            s.status
                .clone()
                .ok_or_else(|| ShepherdError::GithubUnavailable("fake PR status missing".into()))
        })
    }
    fn add_reviewers(
        &self,
        _: &Gate,
        number: u64,
        reviewers: &[String],
    ) -> Result<(), ShepherdError> {
        self.call(format!("reviewers {number} {reviewers:?}"), |_| Ok(()))
    }
    fn reply_id(
        &self,
        gate: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<Option<u64>, ShepherdError> {
        self.reply(gate, number, comment_id, body)?;
        let mut state: DiskState =
            serde_json::from_slice(&std::fs::read(&self.path).map_err(anyhow::Error::from)?)
                .map_err(anyhow::Error::from)?;
        let id = state
            .comments
            .iter()
            .map(|comment| comment.id)
            .max()
            .unwrap_or(0)
            .max(1_000_000)
            + 1;
        state.comments.push(PrComment {
            kind: None,
            id,
            author: state.author.clone(),
            path: None,
            line: None,
            body: body.into(),
            review: comment_id.is_some(),
        });
        std::fs::write(
            &self.path,
            serde_json::to_vec_pretty(&state).map_err(anyhow::Error::from)?,
        )
        .map_err(anyhow::Error::from)?;
        Ok(Some(id))
    }

    fn reply(
        &self,
        _: &Gate,
        number: u64,
        comment_id: Option<u64>,
        body: &str,
    ) -> Result<(), ShepherdError> {
        self.call(format!("reply {number} {comment_id:?} {body}"), |_| Ok(()))
    }
}
