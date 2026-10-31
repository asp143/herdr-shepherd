pub mod cli;
pub mod model;
pub mod socket;

pub use cli::CliHerdr;
pub use model::{AgentInfo, Workspace, WorktreeInfo};

use crate::error::ShepherdError;

pub trait Herdr {
    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError>;
    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError>;
    fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError>;
    fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError>;
}
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
