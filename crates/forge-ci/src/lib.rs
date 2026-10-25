//! CI for omaforge: workflow files (a subset of GitHub Actions), expressions, and the
//! protocol between the server and `omaforge-runner`.

pub mod expr;
pub mod glob;
pub mod protocol;
pub mod workflow;

pub use workflow::{Event, Job, Step, Workflow};
