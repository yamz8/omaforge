//! Talking to the server: claiming jobs, and reporting steps, logs and results.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use forge_ci::protocol::{
    self, ClaimRequest, Conclusion, Finish, JobSpec, JobState, LogChunk, StepUpdate,
};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, Notify};

use crate::Label;

pub struct Runner {
    http: reqwest::Client,
    api: String,
    token: String,
}

impl Runner {
    pub fn new(server: &str, token: &str) -> Result<Self> {
        if !token.starts_with(protocol::RUNNER_TOKEN_PREFIX) {
            bail!(
                "that isn't a runner token (they start with {})",
                protocol::RUNNER_TOKEN_PREFIX
            );
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("omaforge-runner/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Runner {
            http,
            api: format!("{server}{}", protocol::PATH),
            token: token.to_owned(),
        })
    }

    /// Wait for a job (the server holds the request up to about 30 seconds).
    pub async fn claim(&self, labels: &[Label]) -> Result<Option<JobSpec>> {
        let request = ClaimRequest {
            labels: labels.iter().map(|l| l.name.clone()).collect(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let response = self
            .http
            .post(format!("{}/claim", self.api))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(60))
            .json(&request)
            .send()
            .await?;
        match response.status() {
            StatusCode::OK => Ok(Some(response.json().await.context("reading the job")?)),
            StatusCode::NO_CONTENT => Ok(None),
            StatusCode::UNAUTHORIZED => bail!("the server doesn't accept this runner token"),
            status => bail!("the server answered {status}"),
        }
    }

    pub fn job(&self, spec: &JobSpec) -> JobClient {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(Notify::new());
        let masks = Arc::new(Mutex::new(vec![spec.token.clone()]));
        let base = JobBase {
            http: self.http.clone(),
            api: self.api.clone(),
            token: spec.token.clone(),
            cancelled: cancelled.clone(),
            cancel_notify: cancel_notify.clone(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let logs = tokio::spawn(send_logs(base.clone(), rx));
        let heartbeat = tokio::spawn({
            let base = base.clone();
            async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = base.post("heartbeat", &serde_json::json!({})).await;
                }
            }
        });
        JobClient {
            base,
            masks,
            log_tx: Some(tx),
            logs: Some(logs),
            heartbeat,
        }
    }
}

#[derive(Clone)]
struct JobBase {
    http: reqwest::Client,
    api: String,
    token: String,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

impl JobBase {
    /// Post a job call, retrying a few times, and note whether to cancel.
    async fn post<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let mut last = None;
        for attempt in 0..5u64 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(attempt * 2)).await;
            }
            let result = self
                .http
                .post(format!("{}/job/{path}", self.api))
                .bearer_auth(&self.token)
                .timeout(Duration::from_secs(30))
                .json(body)
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => {
                    if let Ok(state) = response.json::<JobState>().await {
                        if state.cancel && !self.cancelled.swap(true, Ordering::SeqCst) {
                            self.cancel_notify.notify_waiters();
                        }
                    }
                    return Ok(());
                }
                // The job is over on the server (cancelled, or timed out): stop.
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    if !self.cancelled.swap(true, Ordering::SeqCst) {
                        self.cancel_notify.notify_waiters();
                    }
                    bail!("the server no longer accepts this job");
                }
                Ok(response) => {
                    last = Some(anyhow::anyhow!("the server answered {}", response.status()))
                }
                Err(error) => last = Some(error.into()),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("couldn't reach the server")))
    }
}

/// Batch log lines into a request every half second (or 64 KB).
async fn send_logs(base: JobBase, mut rx: mpsc::UnboundedReceiver<(usize, String)>) {
    let mut step = 0;
    let mut buffer = String::new();
    loop {
        let next = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        let closed = matches!(next, Ok(None));
        if let Ok(Some((s, text))) = next {
            if s != step && !buffer.is_empty() {
                let _ = base
                    .post(
                        "logs",
                        &LogChunk {
                            step,
                            text: std::mem::take(&mut buffer),
                        },
                    )
                    .await;
            }
            step = s;
            buffer.push_str(&text);
            if buffer.len() < 64 * 1024 {
                continue;
            }
        }
        if !buffer.is_empty() {
            let _ = base
                .post(
                    "logs",
                    &LogChunk {
                        step,
                        text: std::mem::take(&mut buffer),
                    },
                )
                .await;
        }
        if closed {
            return;
        }
    }
}

/// Reporting for one job.
pub struct JobClient {
    base: JobBase,
    masks: Arc<Mutex<Vec<String>>>,
    log_tx: Option<mpsc::UnboundedSender<(usize, String)>>,
    logs: Option<tokio::task::JoinHandle<()>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl JobClient {
    pub fn cancelled(&self) -> bool {
        self.base.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves when the job is cancelled (immediately if it already is).
    pub async fn wait_cancelled(&self) {
        let notified = self.base.cancel_notify.notified();
        if self.cancelled() {
            return;
        }
        notified.await;
    }

    /// Hide `secret` in all later log output (`::add-mask::`).
    pub fn mask(&self, secret: &str) {
        if secret.len() >= 3 {
            self.masks.lock().unwrap().push(secret.to_owned());
        }
    }

    /// Add a line (or more) to a step's log, with masked values hidden.
    pub fn log(&self, step: usize, text: &str) {
        let mut text = text.to_owned();
        for secret in self.masks.lock().unwrap().iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), "***");
            }
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if let Some(tx) = &self.log_tx {
            let _ = tx.send((step, text));
        }
    }

    pub async fn step_started(&self, number: usize) {
        self.flush_soon().await;
        if let Err(error) = self
            .base
            .post(
                "steps",
                &StepUpdate {
                    number,
                    conclusion: None,
                },
            )
            .await
        {
            tracing::warn!("couldn't report step {number}: {error:#}");
        }
    }

    pub async fn step_finished(&self, number: usize, conclusion: Conclusion) {
        self.flush_soon().await;
        if let Err(error) = self
            .base
            .post(
                "steps",
                &StepUpdate {
                    number,
                    conclusion: Some(conclusion),
                },
            )
            .await
        {
            tracing::warn!("couldn't report step {number}: {error:#}");
        }
    }

    /// Give buffered log lines a moment to go out before a step changes state.
    async fn flush_soon(&self) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    pub async fn finish(mut self, conclusion: Conclusion, outputs: Map<String, Value>) {
        // Send every remaining log line before the job closes.
        self.log_tx = None;
        if let Some(logs) = self.logs.take() {
            let _ = logs.await;
        }
        self.heartbeat.abort();
        if let Err(error) = self
            .base
            .post(
                "finish",
                &Finish {
                    conclusion,
                    outputs,
                },
            )
            .await
        {
            tracing::warn!("couldn't report the job's result: {error:#}");
        }
    }
}
