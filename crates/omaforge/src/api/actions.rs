//! GitHub's REST endpoints for Actions (`gh run list/view/cancel/rerun`) and for commit
//! statuses that other CI systems report.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{error, user_json, viewer};
use crate::{
    auth::{self, Access},
    ci, db, web, AppState,
};

pub fn routes() -> Router<AppState> {
    let r = |path: &str| format!("/api/v3/repos/{{owner}}/{{repo}}{path}");
    Router::new()
        .route(&r("/actions/runs"), get(runs))
        .route(&r("/actions/runs/{id}"), get(run))
        .route(
            &r("/actions/runs/{id}/attempts/{attempt}"),
            get(run_attempt),
        )
        .route(&r("/actions/runs/{id}/jobs"), get(jobs))
        .route(
            &r("/actions/runs/{id}/attempts/{attempt}/jobs"),
            get(attempt_jobs),
        )
        .route(&r("/actions/runs/{id}/cancel"), post(cancel))
        .route(&r("/actions/runs/{id}/rerun"), post(rerun))
        .route(&r("/actions/jobs/{id}"), get(job))
        .route(&r("/actions/jobs/{id}/logs"), get(job_logs))
        .route(&r("/actions/workflows"), get(workflows))
        .route(&r("/actions/workflows/{id}"), get(workflow))
        .route(&r("/check-runs/{id}/annotations"), get(no_annotations))
        .route(&r("/actions/runs/{id}/artifacts"), get(no_artifacts))
        .route(&r("/actions/runs/{id}/logs"), get(run_logs))
        .route(
            &r("/actions/runs/{id}/attempts/{attempt}/logs"),
            get(attempt_logs),
        )
        .route(
            &r("/statuses/{sha}"),
            post(create_status).get(list_statuses),
        )
        .route(&r("/commits/{rev}/statuses"), get(list_statuses))
        .route(&r("/commits/{rev}/status"), get(combined_status))
        .route(&r("/commits/{rev}/check-runs"), get(check_runs))
}

struct Repo {
    repo: db::Repo,
    access: Access,
    user: Option<db::User>,
}

#[allow(clippy::result_large_err)]
async fn load(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<Repo, Response> {
    let user = viewer(state, headers).await?;
    let repo = match db::repo_by_path(&state.db, owner, name).await {
        Ok(Some(repo)) => repo,
        Ok(None) => return Err(error(StatusCode::NOT_FOUND, "Not Found")),
        Err(_) => return Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Server error")),
    };
    let access = auth::access(&state.db, user.as_ref(), &repo)
        .await
        .unwrap_or(Access::None);
    if access < Access::Read {
        return Err(error(StatusCode::NOT_FOUND, "Not Found"));
    }
    Ok(Repo { repo, access, user })
}

fn server_error(e: anyhow::Error) -> Response {
    tracing::error!("API: {e:#}");
    error(StatusCode::INTERNAL_SERVER_ERROR, "Server error")
}

#[derive(Deserialize, Default)]
pub struct Page {
    per_page: Option<i64>,
    page: Option<i64>,
    branch: Option<String>,
    event: Option<String>,
    status: Option<String>,
    head_sha: Option<String>,
}

impl Page {
    fn limits(&self) -> (i64, i64) {
        let per_page = self.per_page.unwrap_or(30).clamp(1, 100);
        (per_page, (self.page.unwrap_or(1).max(1) - 1) * per_page)
    }
}

async fn runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<Page>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    // Filters apply to the newest runs; plenty for `gh run list`.
    let all = match db::runs(&state.db, repo.repo.id, None, 1000, 0).await {
        Ok(runs) => runs,
        Err(e) => return server_error(e),
    };
    let matching: Vec<&db::Run> = all
        .iter()
        .filter(|r| {
            query
                .branch
                .as_ref()
                .is_none_or(|b| r.head_branch.as_ref() == Some(b))
        })
        .filter(|r| query.event.as_ref().is_none_or(|e| &r.event == e))
        .filter(|r| query.head_sha.as_ref().is_none_or(|s| &r.head_sha == s))
        .filter(|r| {
            query.status.as_deref().is_none_or(|s| {
                r.status == s
                    || r.conclusion.as_deref() == Some(s)
                    || (s == "pending" && r.status != "completed")
            })
        })
        .collect();
    let (per_page, offset) = query.limits();
    let page: Vec<Value> = matching
        .iter()
        .skip(offset as usize)
        .take(per_page as usize)
        .map(|r| ci::run_json(&state, &repo.repo, r))
        .collect();
    Json(json!({ "total_count": matching.len(), "workflow_runs": page })).into_response()
}

#[allow(clippy::result_large_err)]
async fn load_run(state: &AppState, repo: &Repo, id: i64) -> Result<db::Run, Response> {
    match db::repo_run(&state.db, repo.repo.id, id).await {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(error(StatusCode::NOT_FOUND, "Not Found")),
        Err(e) => Err(server_error(e)),
    }
}

async fn run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    match load_run(&state, &repo, id).await {
        Ok(run) => Json(ci::run_json(&state, &repo.repo, &run)).into_response(),
        Err(response) => response,
    }
}

/// omaforge has one attempt per run (re-runs are new runs).
async fn run_attempt(
    state: State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id, attempt)): Path<(String, String, i64, i64)>,
) -> Response {
    if attempt != 1 {
        return error(StatusCode::NOT_FOUND, "Not Found");
    }
    run(state, headers, Path((owner, name, id))).await
}

fn job_json(
    state: &AppState,
    repo: &db::Repo,
    run: &db::Run,
    job: &db::CiJob,
    steps: &[db::CiStep],
) -> Value {
    let repo_url = format!("{}/{}/{}", state.site.http_url, repo.owner_name, repo.name);
    let api = format!(
        "{}/api/v3/repos/{}/{}",
        state.site.http_url, repo.owner_name, repo.name
    );
    let status = |s: &str| if s == "waiting" { "waiting" } else { s }.to_owned();
    json!({
        "id": job.id,
        "run_id": run.id,
        "run_url": format!("{api}/actions/runs/{}", run.id),
        "node_id": format!("CR_{}", job.id),
        "head_sha": run.head_sha,
        "head_branch": run.head_branch,
        "url": format!("{api}/actions/jobs/{}", job.id),
        "html_url": format!("{repo_url}/actions/runs/{}/jobs/{}", run.id, job.id),
        "status": status(&job.status),
        "conclusion": job.conclusion,
        "created_at": web::render::iso8601(job.created),
        "started_at": web::render::iso8601(job.started.unwrap_or(job.created)),
        "completed_at": job.completed.map(web::render::iso8601),
        "name": job.name,
        "workflow_name": run.workflow_name,
        "run_attempt": 1,
        "labels": job.labels().collect::<Vec<_>>(),
        "runner_name": job.runner,
        "steps": steps.iter().map(|s| json!({
            "name": s.name,
            "status": s.status,
            "conclusion": s.conclusion,
            "number": s.number + 1,
            "started_at": s.started.map(web::render::iso8601),
            "completed_at": s.completed.map(web::render::iso8601),
        })).collect::<Vec<_>>(),
    })
}

async fn jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let run = match load_run(&state, &repo, id).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let result: anyhow::Result<Vec<Value>> = async {
        let mut out = Vec::new();
        for job in db::jobs(&state.db, run.id).await? {
            let steps = db::steps(&state.db, job.id).await?;
            out.push(job_json(&state, &repo.repo, &run, &job, &steps));
        }
        Ok(out)
    }
    .await;
    match result {
        Ok(jobs) => Json(json!({ "total_count": jobs.len(), "jobs": jobs })).into_response(),
        Err(e) => server_error(e),
    }
}

async fn attempt_jobs(
    state: State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id, attempt)): Path<(String, String, i64, i64)>,
) -> Response {
    if attempt != 1 {
        return error(StatusCode::NOT_FOUND, "Not Found");
    }
    jobs(state, headers, Path((owner, name, id))).await
}

#[allow(clippy::result_large_err)]
async fn load_job(
    state: &AppState,
    repo: &Repo,
    id: i64,
) -> Result<(db::Run, db::CiJob), Response> {
    let job = match db::job(&state.db, id).await {
        Ok(Some(job)) => job,
        Ok(None) => return Err(error(StatusCode::NOT_FOUND, "Not Found")),
        Err(e) => return Err(server_error(e)),
    };
    let run = load_run(state, repo, job.run_id).await?;
    Ok((run, job))
}

async fn job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let (run, job) = match load_job(&state, &repo, id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    match db::steps(&state.db, job.id).await {
        Ok(steps) => Json(job_json(&state, &repo.repo, &run, &job, &steps)).into_response(),
        Err(e) => server_error(e),
    }
}

async fn job_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let (_, job) = match load_job(&state, &repo, id).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let text = async {
        let steps = db::steps(&state.db, job.id).await?;
        ci::logs::full(&state, job.id, &steps).await
    }
    .await;
    match text {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(e) => server_error(e),
    }
}

/// A run's logs as GitHub packages them (what `gh run view --log` reads): a zip with each
/// job's whole log as `<n>_<job>.txt` and each step's as `<job>/<step>_<name>.txt`, with
/// `/` and `:` removed from names.
async fn run_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let run = match load_run(&state, &repo, id).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    let clean = |s: &str| -> String {
        let s: String = s.chars().filter(|c| *c != '/' && *c != ':').collect();
        s.chars().take(90).collect()
    };
    let entries = async {
        let mut entries = Vec::new();
        for (i, job) in db::jobs(&state.db, run.id).await?.iter().enumerate() {
            let steps = db::steps(&state.db, job.id).await?;
            let job_name = clean(&job.name);
            entries.push((
                format!("{i}_{job_name}.txt"),
                ci::logs::full(&state, job.id, &steps).await?.into_bytes(),
            ));
            for step in &steps {
                let (text, _) = ci::logs::read(&state, job.id, step.number as usize, 0).await?;
                entries.push((
                    format!("{job_name}/{}_{}.txt", step.number + 1, clean(&step.name)),
                    text.into_bytes(),
                ));
            }
        }
        anyhow::Ok(entries)
    }
    .await;
    match entries {
        Ok(entries) => (
            [
                (header::CONTENT_TYPE, "application/zip".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"logs_{}.zip\"", run.id),
                ),
            ],
            super::zip::stored(&entries),
        )
            .into_response(),
        Err(e) => server_error(e),
    }
}

async fn attempt_logs(
    state: State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id, attempt)): Path<(String, String, i64, i64)>,
) -> Response {
    if attempt != 1 {
        return error(StatusCode::NOT_FOUND, "Not Found");
    }
    run_logs(state, headers, Path((owner, name, id))).await
}

/// Write access, for cancelling and re-running.
#[allow(clippy::result_large_err)]
fn writer(repo: &Repo) -> Result<&db::User, Response> {
    match &repo.user {
        Some(user) if repo.access >= Access::Write => Ok(user),
        Some(_) => Err(error(
            StatusCode::FORBIDDEN,
            "Must have write access to this repository.",
        )),
        None => Err(error(StatusCode::UNAUTHORIZED, "Requires authentication")),
    }
}

async fn cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    if let Err(response) = writer(&repo) {
        return response;
    }
    let run = match load_run(&state, &repo, id).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    if run.status == "completed" {
        return error(
            StatusCode::CONFLICT,
            "Cannot cancel a workflow run that is completed.",
        );
    }
    match ci::cancel(&state, run.id).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({}))).into_response(),
        Err(e) => server_error(e),
    }
}

async fn rerun(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let user = match writer(&repo) {
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    let run = match load_run(&state, &repo, id).await {
        Ok(run) => run,
        Err(response) => return response,
    };
    if run.status != "completed" {
        return error(
            StatusCode::FORBIDDEN,
            "This workflow run is not completed yet.",
        );
    }
    match ci::rerun(&state, &repo.repo, &run, &user).await {
        Ok(Some(_)) => (StatusCode::CREATED, Json(json!({}))).into_response(),
        Ok(None) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The workflow file no longer exists at that commit.",
        ),
        Err(e) => server_error(e),
    }
}

fn workflow_json(state: &AppState, repo: &db::Repo, path: &str, name: &str) -> Value {
    let id = ci::workflow_id(path);
    json!({
        "id": id,
        "node_id": format!("W_{id}"),
        "name": name,
        "path": path,
        "state": "active",
        "url": format!("{}/api/v3/repos/{}/{}/actions/workflows/{id}", state.site.http_url, repo.owner_name, repo.name),
        "html_url": format!("{}/{}/{}/actions?workflow={}", state.site.http_url, repo.owner_name, repo.name, web::form_urlencode(path)),
        "badge_url": "",
    })
}

async fn workflows(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    match db::workflows(&state.db, repo.repo.id).await {
        Ok(list) => {
            let workflows: Vec<Value> = list
                .iter()
                .map(|(path, name)| workflow_json(&state, &repo.repo, path, name))
                .collect();
            Json(json!({ "total_count": workflows.len(), "workflows": workflows })).into_response()
        }
        Err(e) => server_error(e),
    }
}

/// By id, or by file name as GitHub also allows (`ci.yml`).
async fn workflow(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let list = match db::workflows(&state.db, repo.repo.id).await {
        Ok(list) => list,
        Err(e) => return server_error(e),
    };
    let found = list.iter().find(|(path, _)| {
        ci::workflow_id(path).to_string() == id || path.rsplit('/').next() == Some(id.as_str())
    });
    match found {
        Some((path, name)) => Json(workflow_json(&state, &repo.repo, path, name)).into_response(),
        None => error(StatusCode::NOT_FOUND, "Not Found"),
    }
}

/// omaforge doesn't store artifacts (yet).
async fn no_artifacts() -> Json<Value> {
    Json(json!({ "total_count": 0, "artifacts": [] }))
}

async fn no_annotations() -> Json<Value> {
    Json(json!([]))
}

// ---- Commit statuses ----

#[derive(Deserialize)]
pub struct NewStatus {
    state: String,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context: Option<String>,
}

fn status_json(state: &AppState, status: &db::CommitStatus) -> Value {
    json!({
        "id": status.id,
        "node_id": format!("SC_{}", status.id),
        "state": status.state,
        "description": status.description,
        "target_url": (!status.target_url.is_empty()).then_some(&status.target_url),
        "context": status.context,
        "created_at": web::render::iso8601(status.created),
        "updated_at": web::render::iso8601(status.created),
        "creator": status.creator.as_ref().zip(status.creator_id).map(|(n, id)| user_json(state, id, n)),
    })
}

/// Resolve a branch, tag or commit to a full id.
async fn resolve(state: &AppState, repo: &db::Repo, rev: &str) -> Option<String> {
    let git = forge_git::Repository::open(forge_git::repo_path(
        &state.repos,
        &repo.owner_name,
        &repo.name,
    ));
    git.resolve_commit(rev).await.ok().flatten()
}

async fn create_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, sha)): Path<(String, String, String)>,
    body: axum::body::Bytes,
) -> Response {
    // Like GitHub, take JSON whatever the Content-Type says (`curl -d '{...}'`).
    let body: NewStatus = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("Problems parsing JSON: {e}"),
            )
        }
    };
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let user = match writer(&repo) {
        Ok(user) => user.clone(),
        Err(response) => return response,
    };
    if !["error", "failure", "pending", "success"].contains(&body.state.as_str()) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "state must be error, failure, pending or success",
        );
    }
    let Some(sha) = resolve(&state, &repo.repo, &sha).await else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "No commit found for SHA");
    };
    let context = body
        .context
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("default");
    let description = body.description.as_deref().unwrap_or("");
    let target_url = body.target_url.as_deref().unwrap_or("");
    if context.len() > 255 || description.len() > 1024 || target_url.len() > 2048 {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "context, description or target_url is too long",
        );
    }
    let new = db::NewStatus {
        sha: &sha,
        context,
        state: &body.state,
        description,
        target_url,
    };
    let created = async {
        let id = db::add_status(&state.db, repo.repo.id, user.id, &new).await?;
        let statuses = db::statuses(&state.db, repo.repo.id, &sha).await?;
        anyhow::Ok(statuses.into_iter().find(|s| s.id == id))
    }
    .await;
    match created {
        Ok(Some(status)) => {
            let payload = json!({
                "id": status.id,
                "sha": sha,
                "name": format!("{}/{}", repo.repo.owner_name, repo.repo.name),
                "context": status.context,
                "state": status.state,
                "description": status.description,
                "target_url": status.target_url,
                "repository": crate::events::repository(&state, &repo.repo),
            });
            if let Err(e) = crate::events::emit(&state, &repo.repo, "status", &payload).await {
                tracing::warn!("couldn't queue the status webhook: {e:#}");
            }
            (StatusCode::CREATED, Json(status_json(&state, &status))).into_response()
        }
        Ok(None) => error(StatusCode::INTERNAL_SERVER_ERROR, "Server error"),
        Err(e) => server_error(e),
    }
}

async fn list_statuses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev)): Path<(String, String, String)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let Some(sha) = resolve(&state, &repo.repo, &rev).await else {
        return error(StatusCode::NOT_FOUND, "No commit found for SHA");
    };
    match db::statuses(&state.db, repo.repo.id, &sha).await {
        Ok(statuses) => Json(
            statuses
                .iter()
                .map(|s| status_json(&state, s))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => server_error(e),
    }
}

/// The latest status per context, combined: failure if any failed, pending if any are
/// pending (or there are none), else success.
async fn combined_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev)): Path<(String, String, String)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let Some(sha) = resolve(&state, &repo.repo, &rev).await else {
        return error(StatusCode::NOT_FOUND, "No commit found for SHA");
    };
    let statuses = match db::latest_statuses(&state.db, repo.repo.id, &sha).await {
        Ok(statuses) => statuses,
        Err(e) => return server_error(e),
    };
    let combined = if statuses
        .iter()
        .any(|s| s.state == "failure" || s.state == "error")
    {
        "failure"
    } else if statuses.is_empty() || statuses.iter().any(|s| s.state == "pending") {
        "pending"
    } else {
        "success"
    };
    Json(json!({
        "state": combined,
        "sha": sha,
        "total_count": statuses.len(),
        "statuses": statuses.iter().map(|s| status_json(&state, s)).collect::<Vec<_>>(),
        "repository": { "full_name": format!("{}/{}", repo.repo.owner_name, repo.repo.name) },
    }))
    .into_response()
}

/// CI jobs for a commit as GitHub check runs.
async fn check_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev)): Path<(String, String, String)>,
) -> Response {
    let repo = match load(&state, &headers, &owner, &name).await {
        Ok(repo) => repo,
        Err(response) => return response,
    };
    let Some(sha) = resolve(&state, &repo.repo, &rev).await else {
        return error(StatusCode::NOT_FOUND, "No commit found for SHA");
    };
    let checks = match ci::Checks::load(&state, repo.repo.id, &sha).await {
        Ok(checks) => checks,
        Err(e) => return server_error(e),
    };
    let repo_url = format!(
        "{}/{}/{}",
        state.site.http_url, repo.repo.owner_name, repo.repo.name
    );
    let mut runs = Vec::new();
    for (run, jobs) in &checks.runs {
        for job in jobs {
            runs.push(json!({
                "id": job.id,
                "head_sha": sha,
                "name": job.name,
                "status": if job.status == "waiting" { "queued" } else { job.status.as_str() },
                "conclusion": job.conclusion,
                "started_at": job.started.map(web::render::iso8601),
                "completed_at": job.completed.map(web::render::iso8601),
                "html_url": format!("{repo_url}/actions/runs/{}/jobs/{}", run.id, job.id),
                "details_url": format!("{repo_url}/actions/runs/{}/jobs/{}", run.id, job.id),
                "check_suite": { "id": run.id },
                "app": { "slug": "omaforge-actions", "name": "omaforge Actions" },
            }));
        }
    }
    Json(json!({ "total_count": runs.len(), "check_runs": runs })).into_response()
}
