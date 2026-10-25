//! The API `omaforge-runner` talks to (see `forge_ci::protocol`).

use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use forge_ci::protocol::{
    self, ClaimRequest, Finish, JobSpec, JobState, LogChunk, StepUpdate, JOB_TOKEN_PREFIX,
    RUNNER_TOKEN_PREFIX,
};
use serde_json::json;

use crate::{auth, ci, db, AppState};

/// How long a claim waits for a job before answering "nothing yet".
const CLAIM_WAIT: Duration = Duration::from_secs(25);

pub fn routes() -> Router<AppState> {
    let p = |path: &str| format!("{}{path}", protocol::PATH);
    Router::new()
        .route(&p("/claim"), post(claim))
        .route(&p("/job/steps"), post(step))
        .route(&p("/job/logs"), post(log))
        .route(&p("/job/heartbeat"), post(heartbeat))
        .route(&p("/job/finish"), post(finish))
}

fn bearer<'a>(headers: &'a HeaderMap, prefix: &str) -> Option<&'a str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| token.starts_with(prefix))
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "message": message }))).into_response()
}

fn internal(error: anyhow::Error) -> Response {
    tracing::error!("runner API: {error:#}");
    self::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

/// The running job a request's job token belongs to.
#[allow(clippy::result_large_err)]
pub async fn job_for(state: &AppState, headers: &HeaderMap) -> Result<db::CiJob, Response> {
    let Some(token) = bearer(headers, JOB_TOKEN_PREFIX) else {
        return Err(error(StatusCode::UNAUTHORIZED, "a job token is required"));
    };
    match db::job_by_token_hash(&state.db, &auth::hash_token(token)).await {
        Ok(Some(job)) => Ok(job),
        Ok(None) => Err(error(
            StatusCode::UNAUTHORIZED,
            "this job token isn't valid, or the job has finished",
        )),
        Err(e) => Err(internal(e)),
    }
}

async fn claim(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ClaimRequest>,
) -> Response {
    let Some(token) = bearer(&headers, RUNNER_TOKEN_PREFIX) else {
        return error(StatusCode::UNAUTHORIZED, "a runner token is required");
    };
    let runner = match db::runner_by_token_hash(&state.db, &auth::hash_token(token)).await {
        Ok(Some(runner)) => runner,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "unknown runner token"),
        Err(e) => return internal(e),
    };
    let labels: Vec<String> = request
        .labels
        .iter()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty() && !l.contains(','))
        .collect();
    let version: String = request.version.chars().take(40).collect();
    if let Err(e) = db::touch_runner(&state.db, runner.id, &labels.join(","), &version).await {
        return internal(e);
    }

    let deadline = Instant::now() + CLAIM_WAIT;
    loop {
        // Register for a wakeup before looking, so a job queued in between isn't missed.
        let notified = ci::queued().notified();
        match try_claim(&state, runner.id, &labels).await {
            Ok(Some(spec)) => return Json(spec).into_response(),
            Ok(None) => {}
            Err(e) => return internal(e),
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = db::touch_runner(&state.db, runner.id, &labels.join(","), &version).await;
            return StatusCode::NO_CONTENT.into_response();
        }
        // Jobs queued by git hooks run in other processes, so also poll.
        let wait = (deadline - now).min(Duration::from_secs(1));
        let _ = tokio::time::timeout(wait, notified).await;
    }
}

async fn try_claim(
    state: &AppState,
    runner_id: i64,
    labels: &[String],
) -> anyhow::Result<Option<JobSpec>> {
    for (job_id, runs_on) in db::queued_jobs(&state.db).await? {
        let fits = runs_on
            .split(',')
            .filter(|l| !l.is_empty())
            .all(|needed| labels.iter().any(|l| l == needed));
        if !fits {
            continue;
        }
        let (token, hash) = job_token()?;
        if !db::claim_job(&state.db, job_id, runner_id, &hash).await? {
            continue;
        }
        let job = db::job(&state.db, job_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("claimed job vanished"))?;
        db::start_run(&state.db, job.run_id).await?;
        return Ok(Some(spec(state, &job, token).await?));
    }
    Ok(None)
}

fn job_token() -> anyhow::Result<(String, String)> {
    let token = format!("{JOB_TOKEN_PREFIX}{}", auth::random_secret()?);
    let hash = auth::hash_token(&token);
    Ok((token, hash))
}

async fn spec(state: &AppState, job: &db::CiJob, token: String) -> anyhow::Result<JobSpec> {
    let run = db::run(&state.db, job.run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("run missing"))?;
    let repo = db::repo_by_id(&state.db, run.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repository missing"))?;
    let stored = ci::stored_spec(job)?;
    let all = db::jobs(&state.db, job.run_id).await?;
    let mut needs = serde_json::Map::new();
    for key in job.needs() {
        let instances: Vec<&db::CiJob> = all.iter().filter(|j| j.job_key == key).collect();
        let mut outputs = serde_json::Map::new();
        for instance in &instances {
            if let Ok(serde_json::Value::Object(o)) = serde_json::from_str(&instance.outputs) {
                outputs.extend(o);
            }
        }
        needs.insert(
            key.to_owned(),
            json!({ "result": ci::combined_result(instances.iter().map(|j| j.state())), "outputs": outputs }),
        );
    }
    let mut contexts = serde_json::Map::new();
    contexts.insert("github".into(), ci::github_context(state, &repo, &run, job));
    contexts.insert("matrix".into(), stored.matrix);
    contexts.insert("strategy".into(), stored.strategy);
    contexts.insert("needs".into(), serde_json::Value::Object(needs));
    contexts.insert("inputs".into(), json!({}));
    contexts.insert("vars".into(), json!({}));
    contexts.insert("secrets".into(), json!({}));
    Ok(JobSpec {
        id: job.id,
        run_id: run.id,
        name: job.name.clone(),
        token,
        server_url: state.site.http_url.clone(),
        repository: format!("{}/{}", repo.owner_name, repo.name),
        clone_url: format!(
            "{}/{}/{}.git",
            state.site.http_url, repo.owner_name, repo.name
        ),
        sha: run.head_sha,
        git_ref: run.git_ref,
        workflow_env: stored.workflow_env,
        workflow_defaults: stored.workflow_defaults,
        job: stored.job,
        contexts,
    })
}

async fn job_state(state: &AppState, job: &db::CiJob) -> Response {
    match db::heartbeat(&state.db, job.id).await {
        Ok(cancel) => Json(JobState { cancel }).into_response(),
        Err(e) => internal(e),
    }
}

async fn step(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(update): Json<StepUpdate>,
) -> Response {
    let job = match job_for(&state, &headers).await {
        Ok(job) => job,
        Err(response) => return response,
    };
    let conclusion = update.conclusion.map(|c| c.as_str());
    if let Err(e) = db::update_step(&state.db, job.id, update.number as i64, conclusion).await {
        return internal(e);
    }
    job_state(&state, &job).await
}

async fn log(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(chunk): Json<LogChunk>,
) -> Response {
    let job = match job_for(&state, &headers).await {
        Ok(job) => job,
        Err(response) => return response,
    };
    if chunk.step > 1000 {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "no such step");
    }
    ci::logs::append(&state, job.id, chunk.step, &chunk.text).await;
    job_state(&state, &job).await
}

async fn heartbeat(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match job_for(&state, &headers).await {
        Ok(job) => job_state(&state, &job).await,
        Err(response) => response,
    }
}

async fn finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(finish): Json<Finish>,
) -> Response {
    let job = match job_for(&state, &headers).await {
        Ok(job) => job,
        Err(response) => return response,
    };
    // Keep outputs small: they're stored and passed to later jobs.
    let mut outputs = finish.outputs;
    outputs.retain(|key, value| key.len() <= 100 && value.to_string().len() <= 64 * 1024);
    match ci::job_finished(&state, &job, finish.conclusion, &outputs).await {
        Ok(()) => Json(JobState::default()).into_response(),
        Err(e) => internal(e),
    }
}
