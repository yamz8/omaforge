//! What the server and `omaforge-runner` send each other, as JSON over HTTP.
//!
//! A runner authenticates with its runner token (`omr_...`) to claim jobs. Each claimed
//! job comes with its own token (`omj_...`), valid until the job finishes, which the
//! runner uses to report on that job and to clone the repository.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::workflow::{Job, RunDefaults};

/// Under the server's URL.
pub const PATH: &str = "/api/runner/v1";

pub const RUNNER_TOKEN_PREFIX: &str = "omr_";
pub const JOB_TOKEN_PREFIX: &str = "omj_";

/// `POST {PATH}/claim`: wait (up to about 30 seconds) for a job this runner can run.
/// Answers 200 with a [`JobSpec`] or 204 when there was none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// Labels this runner accepts, matched against `runs-on`.
    pub labels: Vec<String>,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: i64,
    pub run_id: i64,
    /// As shown in the UI, e.g. `test (ubuntu-latest, 18)`.
    pub name: String,
    /// The job's own token: send it as `Authorization: Bearer` for job calls and git.
    pub token: String,
    pub server_url: String,
    /// `owner/name`.
    pub repository: String,
    pub clone_url: String,
    pub sha: String,
    pub git_ref: String,
    pub workflow_env: Map<String, Value>,
    pub workflow_defaults: RunDefaults,
    pub job: Job,
    /// `github`, `matrix`, `strategy`, `needs`, `inputs`, `vars`, `secrets`.
    pub contexts: Map<String, Value>,
}

/// Steps are numbered with 0 for setting up the job, the workflow's steps from 1, and
/// one after the last for cleaning up.
pub fn step_count(job: &Job) -> usize {
    job.steps.len() + 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Conclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl Conclusion {
    pub fn as_str(self) -> &'static str {
        match self {
            Conclusion::Success => "success",
            Conclusion::Failure => "failure",
            Conclusion::Cancelled => "cancelled",
            Conclusion::Skipped => "skipped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "success" => Conclusion::Success,
            "failure" => Conclusion::Failure,
            "cancelled" => Conclusion::Cancelled,
            "skipped" => Conclusion::Skipped,
            _ => return None,
        })
    }
}

/// `POST {PATH}/job/steps`: a step started (`conclusion` empty) or finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepUpdate {
    pub number: usize,
    pub conclusion: Option<Conclusion>,
}

/// `POST {PATH}/job/logs`: more output for a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogChunk {
    pub step: usize,
    pub text: String,
}

/// `POST {PATH}/job/finish`: the job is done; its token stops working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finish {
    pub conclusion: Conclusion,
    #[serde(default)]
    pub outputs: Map<String, Value>,
}

/// The answer to every job call (and `POST {PATH}/job/heartbeat`): whether someone asked
/// to cancel the job.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobState {
    pub cancel: bool,
}
