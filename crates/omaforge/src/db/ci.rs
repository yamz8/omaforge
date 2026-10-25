//! CI: runners, workflow runs, their jobs and steps, and commit statuses from elsewhere.

use anyhow::Result;
use sqlx::SqlitePool;

/// Seconds since the epoch, from SQLite's `CURRENT_TIMESTAMP` text.
macro_rules! unix {
    ($column:literal) => {
        concat!("CAST(strftime('%s', ", $column, ") AS INTEGER)")
    };
}

// ---- Runners ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Runner {
    pub id: i64,
    pub name: String,
    pub labels: String,
    pub version: String,
    pub last_seen: Option<i64>,
}

macro_rules! runner_columns {
    () => {
        concat!(
            "id, name, labels, version, ",
            unix!("last_seen_at"),
            " AS last_seen"
        )
    };
}

pub async fn create_runner(db: &SqlitePool, name: &str, token_hash: &str) -> Result<i64> {
    let id =
        sqlx::query_scalar("INSERT INTO runners (name, token_hash) VALUES (?, ?) RETURNING id")
            .bind(name)
            .bind(token_hash)
            .fetch_one(db)
            .await
            .map_err(|error| match error {
                sqlx::Error::Database(e) if e.is_unique_violation() => {
                    anyhow::anyhow!("there's already a runner called {name:?}")
                }
                error => error.into(),
            })?;
    Ok(id)
}

pub async fn runners(db: &SqlitePool) -> Result<Vec<Runner>> {
    let runners = sqlx::query_as(concat!(
        "SELECT ",
        runner_columns!(),
        " FROM runners ORDER BY name"
    ))
    .fetch_all(db)
    .await?;
    Ok(runners)
}

pub async fn runner_by_token_hash(db: &SqlitePool, hash: &str) -> Result<Option<Runner>> {
    let runner = sqlx::query_as(concat!(
        "SELECT ",
        runner_columns!(),
        " FROM runners WHERE token_hash = ?"
    ))
    .bind(hash)
    .fetch_optional(db)
    .await?;
    Ok(runner)
}

pub async fn touch_runner(db: &SqlitePool, id: i64, labels: &str, version: &str) -> Result<()> {
    sqlx::query(
        "UPDATE runners SET labels = ?, version = ?, last_seen_at = CURRENT_TIMESTAMP WHERE id = ?",
    )
    .bind(labels)
    .bind(version)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_runner(db: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM runners WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

// ---- Runs ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Run {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub workflow_name: String,
    pub workflow_path: String,
    pub title: String,
    pub event: String,
    pub git_ref: String,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub pull_number: Option<i64>,
    pub actor_id: Option<i64>,
    pub actor: Option<String>,
    pub payload: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub error: Option<String>,
    pub cancelled: bool,
    pub created: i64,
    pub started: Option<i64>,
    pub completed: Option<i64>,
}

impl Run {
    /// `success`, `failure`, `cancelled`, `skipped`, `in_progress` or `queued`.
    pub fn state(&self) -> &str {
        self.conclusion.as_deref().unwrap_or(&self.status)
    }
}

macro_rules! run_columns {
    () => {
        concat!(
            "w.id, w.repo_id, w.number, w.workflow_name, w.workflow_path, w.title, w.event, w.git_ref, ",
            "w.head_branch, w.head_sha, w.pull_number, w.actor_id, u.username AS actor, w.payload, ",
            "w.status, w.conclusion, w.error, w.cancelled, ",
            unix!("w.created_at"),
            " AS created, ",
            unix!("w.started_at"),
            " AS started, ",
            unix!("w.completed_at"),
            " AS completed FROM workflow_runs w LEFT JOIN users u ON u.id = w.actor_id"
        )
    };
}

pub struct NewRun<'a> {
    pub repo_id: i64,
    pub workflow_name: &'a str,
    pub workflow_path: &'a str,
    pub title: &'a str,
    pub event: &'a str,
    pub git_ref: &'a str,
    pub head_branch: Option<&'a str>,
    pub head_sha: &'a str,
    pub pull_number: Option<i64>,
    pub actor_id: Option<i64>,
    pub payload: &'a str,
    /// The workflow file is invalid: the run is recorded as failed straight away.
    pub error: Option<&'a str>,
}

pub struct NewJob {
    pub key: String,
    pub name: String,
    pub needs: Vec<String>,
    pub runs_on: Vec<String>,
    pub spec: String,
    pub step_names: Vec<String>,
}

/// Record a run and its jobs. Jobs that need nothing are queued; the rest wait.
pub async fn create_run(db: &SqlitePool, run: &NewRun<'_>, jobs: &[NewJob]) -> Result<i64> {
    let mut tx = db.begin().await?;
    let (status, conclusion) = if run.error.is_some() {
        ("completed", Some("failure"))
    } else {
        ("queued", None)
    };
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO workflow_runs (repo_id, number, workflow_name, workflow_path, title, event, git_ref,
             head_branch, head_sha, pull_number, actor_id, payload, status, conclusion, error, completed_at)
         VALUES (?, (SELECT COALESCE(MAX(number), 0) + 1 FROM workflow_runs WHERE repo_id = ?),
             ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CASE WHEN ? IS NULL THEN NULL ELSE CURRENT_TIMESTAMP END)
         RETURNING id",
    )
    .bind(run.repo_id)
    .bind(run.repo_id)
    .bind(run.workflow_name)
    .bind(run.workflow_path)
    .bind(run.title)
    .bind(run.event)
    .bind(run.git_ref)
    .bind(run.head_branch)
    .bind(run.head_sha)
    .bind(run.pull_number)
    .bind(run.actor_id)
    .bind(run.payload)
    .bind(status)
    .bind(conclusion)
    .bind(run.error)
    .bind(run.error)
    .fetch_one(&mut *tx)
    .await?;
    for job in jobs {
        let status = if job.needs.is_empty() {
            "queued"
        } else {
            "waiting"
        };
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO ci_jobs (run_id, job_key, name, needs, runs_on, spec, status)
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(run_id)
        .bind(&job.key)
        .bind(&job.name)
        .bind(job.needs.join(","))
        .bind(job.runs_on.join(","))
        .bind(&job.spec)
        .bind(status)
        .fetch_one(&mut *tx)
        .await?;
        for (number, name) in job.step_names.iter().enumerate() {
            sqlx::query("INSERT INTO ci_steps (job_id, number, name) VALUES (?, ?, ?)")
                .bind(job_id)
                .bind(number as i64)
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(run_id)
}

pub async fn run(db: &SqlitePool, id: i64) -> Result<Option<Run>> {
    let run = sqlx::query_as(concat!("SELECT ", run_columns!(), " WHERE w.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(run)
}

pub async fn repo_run(db: &SqlitePool, repo_id: i64, id: i64) -> Result<Option<Run>> {
    Ok(run(db, id).await?.filter(|run| run.repo_id == repo_id))
}

/// Newest first. `workflow` filters by workflow file path.
pub async fn runs(
    db: &SqlitePool,
    repo_id: i64,
    workflow: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Run>> {
    let runs = sqlx::query_as(concat!(
        "SELECT ",
        run_columns!(),
        " WHERE w.repo_id = ? AND (? IS NULL OR w.workflow_path = ?) ORDER BY w.id DESC LIMIT ? OFFSET ?"
    ))
    .bind(repo_id)
    .bind(workflow)
    .bind(workflow)
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;
    Ok(runs)
}

pub async fn run_count(db: &SqlitePool, repo_id: i64, workflow: Option<&str>) -> Result<i64> {
    let count = sqlx::query_scalar(
        "SELECT COUNT(*) FROM workflow_runs WHERE repo_id = ? AND (? IS NULL OR workflow_path = ?)",
    )
    .bind(repo_id)
    .bind(workflow)
    .bind(workflow)
    .fetch_one(db)
    .await?;
    Ok(count)
}

/// Each workflow seen in the repository's runs: (path, latest name).
pub async fn workflows(db: &SqlitePool, repo_id: i64) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as(
        "SELECT workflow_path, (SELECT workflow_name FROM workflow_runs l
             WHERE l.repo_id = w.repo_id AND l.workflow_path = w.workflow_path ORDER BY l.id DESC LIMIT 1)
         FROM workflow_runs w WHERE repo_id = ? GROUP BY workflow_path ORDER BY workflow_path",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Runs for a commit, newest first.
pub async fn runs_for_sha(db: &SqlitePool, repo_id: i64, sha: &str) -> Result<Vec<Run>> {
    let runs = sqlx::query_as(concat!(
        "SELECT ",
        run_columns!(),
        " WHERE w.repo_id = ? AND w.head_sha = ? ORDER BY w.id DESC"
    ))
    .bind(repo_id)
    .bind(sha)
    .fetch_all(db)
    .await?;
    Ok(runs)
}

pub async fn start_run(db: &SqlitePool, run_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE workflow_runs SET status = 'in_progress', started_at = COALESCE(started_at, CURRENT_TIMESTAMP)
         WHERE id = ? AND status = 'queued'",
    )
    .bind(run_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Returns whether this call completed it (so it's reported once).
pub async fn complete_run(db: &SqlitePool, run_id: i64, conclusion: &str) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE workflow_runs SET status = 'completed', conclusion = ?, completed_at = CURRENT_TIMESTAMP,
             started_at = COALESCE(started_at, CURRENT_TIMESTAMP)
         WHERE id = ? AND status != 'completed'",
    )
    .bind(conclusion)
    .bind(run_id)
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

// ---- Jobs ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CiJob {
    pub id: i64,
    pub run_id: i64,
    pub job_key: String,
    pub name: String,
    pub needs: String,
    pub runs_on: String,
    pub spec: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub outputs: String,
    pub runner: Option<String>,
    pub created: i64,
    pub started: Option<i64>,
    pub completed: Option<i64>,
}

impl CiJob {
    pub fn state(&self) -> &str {
        self.conclusion.as_deref().unwrap_or(&self.status)
    }

    pub fn needs(&self) -> impl Iterator<Item = &str> {
        self.needs.split(',').filter(|s| !s.is_empty())
    }

    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.runs_on.split(',').filter(|s| !s.is_empty())
    }
}

macro_rules! job_columns {
    () => {
        concat!(
            "j.id, j.run_id, j.job_key, j.name, j.needs, j.runs_on, j.spec, j.status, j.conclusion, j.outputs, ",
            "r.name AS runner, ",
            unix!("j.created_at"),
            " AS created, ",
            unix!("j.started_at"),
            " AS started, ",
            unix!("j.completed_at"),
            " AS completed FROM ci_jobs j LEFT JOIN runners r ON r.id = j.runner_id"
        )
    };
}

pub async fn jobs(db: &SqlitePool, run_id: i64) -> Result<Vec<CiJob>> {
    let jobs = sqlx::query_as(concat!(
        "SELECT ",
        job_columns!(),
        " WHERE j.run_id = ? ORDER BY j.id"
    ))
    .bind(run_id)
    .fetch_all(db)
    .await?;
    Ok(jobs)
}

pub async fn repo_job_ids(db: &SqlitePool, repo_id: i64) -> Result<Vec<i64>> {
    let ids = sqlx::query_scalar(
        "SELECT j.id FROM ci_jobs j JOIN workflow_runs w ON w.id = j.run_id WHERE w.repo_id = ?",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    Ok(ids)
}

pub async fn job(db: &SqlitePool, id: i64) -> Result<Option<CiJob>> {
    let job = sqlx::query_as(concat!("SELECT ", job_columns!(), " WHERE j.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(job)
}

/// The running job a job token belongs to.
pub async fn job_by_token_hash(db: &SqlitePool, hash: &str) -> Result<Option<CiJob>> {
    let job = sqlx::query_as(concat!(
        "SELECT ",
        job_columns!(),
        " WHERE j.token_hash = ? AND j.status = 'in_progress'"
    ))
    .bind(hash)
    .fetch_optional(db)
    .await?;
    Ok(job)
}

/// Queued jobs, oldest first: (id, runs_on).
pub async fn queued_jobs(db: &SqlitePool) -> Result<Vec<(i64, String)>> {
    let jobs = sqlx::query_as(
        "SELECT id, runs_on FROM ci_jobs WHERE status = 'queued' ORDER BY id LIMIT 500",
    )
    .fetch_all(db)
    .await?;
    Ok(jobs)
}

/// Give a queued job to a runner. False if another runner got it first.
pub async fn claim_job(
    db: &SqlitePool,
    job_id: i64,
    runner_id: i64,
    token_hash: &str,
) -> Result<bool> {
    let claimed = sqlx::query(
        "UPDATE ci_jobs SET status = 'in_progress', runner_id = ?, token_hash = ?,
             started_at = CURRENT_TIMESTAMP, heartbeat_at = CURRENT_TIMESTAMP
         WHERE id = ? AND status = 'queued'",
    )
    .bind(runner_id)
    .bind(token_hash)
    .bind(job_id)
    .execute(db)
    .await?;
    Ok(claimed.rows_affected() == 1)
}

/// The runner is still working on the job. Returns whether it should cancel.
pub async fn heartbeat(db: &SqlitePool, job_id: i64) -> Result<bool> {
    let cancel = sqlx::query_scalar(
        "UPDATE ci_jobs SET heartbeat_at = CURRENT_TIMESTAMP WHERE id = ? RETURNING cancel_requested",
    )
    .bind(job_id)
    .fetch_optional(db)
    .await?;
    Ok(cancel.unwrap_or(false))
}

/// Move a job that isn't running yet (waiting or queued) to `status`.
pub async fn set_pending_job(
    db: &SqlitePool,
    job_id: i64,
    status: &str,
    conclusion: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE ci_jobs SET status = ?, conclusion = ?,
             completed_at = CASE WHEN ? = 'completed' THEN CURRENT_TIMESTAMP END
         WHERE id = ? AND status IN ('waiting', 'queued')",
    )
    .bind(status)
    .bind(conclusion)
    .bind(status)
    .bind(job_id)
    .execute(db)
    .await?;
    if status == "completed" {
        sqlx::query(
            "UPDATE ci_steps SET status = 'completed', conclusion = 'skipped' WHERE job_id = ?",
        )
        .bind(job_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Returns whether this call finished it.
pub async fn finish_job(
    db: &SqlitePool,
    job_id: i64,
    conclusion: &str,
    outputs: &str,
) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE ci_jobs SET status = 'completed', conclusion = ?, outputs = ?, token_hash = NULL,
             completed_at = CURRENT_TIMESTAMP
         WHERE id = ? AND status = 'in_progress'",
    )
    .bind(conclusion)
    .bind(outputs)
    .bind(job_id)
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    // Steps the runner never got to.
    let unfinished = if conclusion == "success" {
        "skipped"
    } else {
        conclusion
    };
    sqlx::query(
        "UPDATE ci_steps SET status = 'completed', completed_at = CURRENT_TIMESTAMP,
             conclusion = CASE WHEN status = 'queued' THEN 'skipped' ELSE ? END
         WHERE job_id = ? AND status != 'completed'",
    )
    .bind(unfinished)
    .bind(job_id)
    .execute(db)
    .await?;
    Ok(true)
}

/// Cancel a run: jobs not started yet are cancelled now; running ones are asked to stop.
pub async fn cancel_run(db: &SqlitePool, run_id: i64) -> Result<()> {
    sqlx::query("UPDATE workflow_runs SET cancelled = 1 WHERE id = ? AND status != 'completed'")
        .bind(run_id)
        .execute(db)
        .await?;
    let pending: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM ci_jobs WHERE run_id = ? AND status IN ('waiting', 'queued')",
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;
    for id in pending {
        set_pending_job(db, id, "completed", Some("cancelled")).await?;
    }
    sqlx::query(
        "UPDATE ci_jobs SET cancel_requested = 1 WHERE run_id = ? AND status = 'in_progress'",
    )
    .bind(run_id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn request_cancel_job(db: &SqlitePool, job_id: i64) -> Result<()> {
    sqlx::query("UPDATE ci_jobs SET cancel_requested = 1 WHERE id = ? AND status = 'in_progress'")
        .bind(job_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Running jobs whose runner hasn't been heard from in `seconds`.
pub async fn stale_jobs(db: &SqlitePool, seconds: i64) -> Result<Vec<i64>> {
    let jobs = sqlx::query_scalar(
        "SELECT id FROM ci_jobs WHERE status = 'in_progress'
             AND heartbeat_at < datetime('now', '-' || ? || ' seconds')",
    )
    .bind(seconds)
    .fetch_all(db)
    .await?;
    Ok(jobs)
}

// ---- Steps ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CiStep {
    pub number: i64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub started: Option<i64>,
    pub completed: Option<i64>,
}

impl CiStep {
    pub fn state(&self) -> &str {
        self.conclusion.as_deref().unwrap_or(&self.status)
    }
}

pub async fn steps(db: &SqlitePool, job_id: i64) -> Result<Vec<CiStep>> {
    let steps = sqlx::query_as(concat!(
        "SELECT number, name, status, conclusion, ",
        unix!("started_at"),
        " AS started, ",
        unix!("completed_at"),
        " AS completed FROM ci_steps WHERE job_id = ? ORDER BY number"
    ))
    .bind(job_id)
    .fetch_all(db)
    .await?;
    Ok(steps)
}

/// A step started (no conclusion) or finished.
pub async fn update_step(
    db: &SqlitePool,
    job_id: i64,
    number: i64,
    conclusion: Option<&str>,
) -> Result<()> {
    match conclusion {
        None => sqlx::query(
            "UPDATE ci_steps SET status = 'in_progress', started_at = CURRENT_TIMESTAMP
             WHERE job_id = ? AND number = ? AND status = 'queued'",
        ),
        Some(_) => sqlx::query(
            "UPDATE ci_steps SET status = 'completed', conclusion = ?, completed_at = CURRENT_TIMESTAMP,
                 started_at = COALESCE(started_at, CURRENT_TIMESTAMP)
             WHERE job_id = ? AND number = ? AND status != 'completed'",
        )
        .bind(conclusion),
    }
    .bind(job_id)
    .bind(number)
    .execute(db)
    .await?;
    Ok(())
}

// ---- Commit statuses ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CommitStatus {
    pub id: i64,
    pub context: String,
    pub state: String,
    pub description: String,
    pub target_url: String,
    pub creator_id: Option<i64>,
    pub creator: Option<String>,
    pub created: i64,
}

pub struct NewStatus<'a> {
    pub sha: &'a str,
    pub context: &'a str,
    pub state: &'a str,
    pub description: &'a str,
    pub target_url: &'a str,
}

pub async fn add_status(
    db: &SqlitePool,
    repo_id: i64,
    creator_id: i64,
    status: &NewStatus<'_>,
) -> Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO commit_statuses (repo_id, sha, context, state, description, target_url, creator_id)
         VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(repo_id)
    .bind(status.sha)
    .bind(status.context)
    .bind(status.state)
    .bind(status.description)
    .bind(status.target_url)
    .bind(creator_id)
    .fetch_one(db)
    .await?;
    Ok(id)
}

macro_rules! status_columns {
    () => {
        concat!(
            "s.id, s.context, s.state, s.description, s.target_url, s.creator_id, u.username AS creator, ",
            unix!("s.created_at"),
            " AS created FROM commit_statuses s LEFT JOIN users u ON u.id = s.creator_id"
        )
    };
}

/// Every status for a commit, newest first.
pub async fn statuses(db: &SqlitePool, repo_id: i64, sha: &str) -> Result<Vec<CommitStatus>> {
    let statuses = sqlx::query_as(concat!(
        "SELECT ",
        status_columns!(),
        " WHERE s.repo_id = ? AND s.sha = ? ORDER BY s.id DESC LIMIT 1000"
    ))
    .bind(repo_id)
    .bind(sha)
    .fetch_all(db)
    .await?;
    Ok(statuses)
}

/// The latest status per context, by context name.
pub async fn latest_statuses(
    db: &SqlitePool,
    repo_id: i64,
    sha: &str,
) -> Result<Vec<CommitStatus>> {
    let statuses = sqlx::query_as(concat!(
        "SELECT ",
        status_columns!(),
        " WHERE s.id IN (SELECT MAX(id) FROM commit_statuses WHERE repo_id = ? AND sha = ? GROUP BY context)
          ORDER BY s.context"
    ))
    .bind(repo_id)
    .bind(sha)
    .fetch_all(db)
    .await?;
    Ok(statuses)
}
