//! CI: start workflow runs for pushes and pull requests, hand their jobs to runners, and
//! move runs along as jobs finish.
//!
//! Workflow files live in `.omaforge/workflows/` or `.github/workflows/` (so repositories
//! from GitHub keep working). A run's jobs are queued, or wait for the jobs they `need`;
//! `omaforge-runner` claims queued jobs whose `runs-on` labels it has (see `runner_api`).

pub mod logs;
pub mod runner_api;

use std::{sync::OnceLock, time::Duration};

use anyhow::{Context as _, Result};
use forge_ci::{
    expr::{self, Context, Status},
    protocol::{self, Conclusion},
    Event, Job, Workflow,
};
use forge_git::{ObjectKind, Repository};
use serde_json::{json, Map, Value};
use tokio::sync::Notify;

use crate::{db, events, web, AppState};

const WORKFLOW_DIRS: &[&str] = &[".omaforge/workflows", ".github/workflows"];
const MAX_WORKFLOW_SIZE: u64 = 512 * 1024;
/// A running job whose runner is silent this long is failed. Tests shorten it with
/// `OMAFORGE_RUNNER_TIMEOUT` (seconds).
fn runner_timeout() -> i64 {
    std::env::var("OMAFORGE_RUNNER_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s: &i64| s > 0)
        .unwrap_or(120)
}

/// Wakes waiting runners when jobs are queued from this process. Jobs queued by git
/// hooks (other processes) are found by polling.
pub fn queued() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// A new runner token and its hash (only the hash is stored).
pub fn new_runner_token() -> Result<(String, String)> {
    let token = format!(
        "{}{}",
        protocol::RUNNER_TOKEN_PREFIX,
        crate::auth::random_secret()?
    );
    let hash = crate::auth::hash_token(&token);
    Ok((token, hash))
}

/// What a run is for. Everything a re-run needs is stored with the run.
pub struct Trigger<'a> {
    pub event: &'a str,
    /// `pull_request` only: `opened`, `synchronize`, ...
    pub action: Option<&'a str>,
    pub git_ref: String,
    pub sha: String,
    pub head_branch: Option<String>,
    pub base_branch: Option<String>,
    pub pull_number: Option<i64>,
    pub title: String,
    pub payload: &'a Value,
    /// The commit to compare against for `paths` filters, if any.
    pub compare_from: Option<String>,
}

fn git(state: &AppState, repo: &db::Repo) -> Repository {
    Repository::open(forge_git::repo_path(
        &state.repos,
        &repo.owner_name,
        &repo.name,
    ))
}

/// Start the workflows `push` and `pull_request` events trigger. Failures are logged:
/// CI must never break a push.
pub async fn on_event(
    state: &AppState,
    repo: &db::Repo,
    event: &str,
    payload: &Value,
    actor: &db::User,
) {
    if let Err(error) = trigger_event(state, repo, event, payload, actor).await {
        tracing::warn!(repo = %format!("{}/{}", repo.owner_name, repo.name), event, "couldn't start CI: {error:#}");
    }
}

async fn trigger_event(
    state: &AppState,
    repo: &db::Repo,
    event: &str,
    payload: &Value,
    actor: &db::User,
) -> Result<()> {
    let text = |v: &Value| v.as_str().map(str::to_owned);
    let trigger = match event {
        "push" => {
            if payload["deleted"] == json!(true) {
                return Ok(());
            }
            let git_ref = text(&payload["ref"]).context("push without a ref")?;
            let before = text(&payload["before"]).unwrap_or_default();
            let after = text(&payload["after"]).context("push without a commit")?;
            // A new branch counts as changing what it adds to the default branch.
            let compare_from = if !crate::hooks::is_zero(&before) && !before.is_empty() {
                Some(before)
            } else {
                let git = git(state, repo);
                match git.default_branch().await.ok().flatten() {
                    Some(default) => match git
                        .resolve_commit(&format!("refs/heads/{default}"))
                        .await
                        .ok()
                        .flatten()
                    {
                        Some(tip) if tip != after => {
                            git.merge_base(&tip, &after).await.ok().flatten()
                        }
                        _ => None,
                    },
                    None => None,
                }
            };
            Trigger {
                event,
                action: None,
                head_branch: git_ref.strip_prefix("refs/heads/").map(str::to_owned),
                base_branch: None,
                pull_number: None,
                sha: after,
                title: text(&payload["head_commit"]["message"])
                    .and_then(|m| m.lines().next().map(str::to_owned))
                    .unwrap_or_else(|| git_ref.clone()),
                compare_from,
                git_ref,
                payload,
            }
        }
        "pull_request" => {
            let action = payload["action"].as_str().unwrap_or("");
            let pull = &payload["pull_request"];
            let number = pull["number"]
                .as_i64()
                .context("pull request without a number")?;
            let base_sha = text(&pull["base"]["sha"]);
            let head_sha = text(&pull["head"]["sha"]).context("pull request without a head")?;
            let compare_from = match base_sha {
                Some(base) => git(state, repo)
                    .merge_base(&base, &head_sha)
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            Trigger {
                event,
                action: Some(action),
                git_ref: format!("refs/pull/{number}/head"),
                sha: head_sha,
                head_branch: text(&pull["head"]["ref"]),
                base_branch: text(&pull["base"]["ref"]),
                pull_number: Some(number),
                title: text(&pull["title"]).unwrap_or_default(),
                payload,
                compare_from,
            }
        }
        _ => return Ok(()),
    };
    start_runs(state, repo, &trigger, Some(actor), None).await?;
    Ok(())
}

/// Start each workflow at the trigger's commit that it triggers. With `only`, start just
/// that workflow file, whatever its `on` says (a re-run). Returns the new run ids.
pub async fn start_runs(
    state: &AppState,
    repo: &db::Repo,
    trigger: &Trigger<'_>,
    actor: Option<&db::User>,
    only: Option<&str>,
) -> Result<Vec<i64>> {
    let git = git(state, repo);
    let mut files = Vec::new();
    for dir in WORKFLOW_DIRS {
        if git.object_kind(&trigger.sha, dir).await? != Some(ObjectKind::Tree) {
            continue;
        }
        for entry in git.tree(&trigger.sha, dir).await? {
            let path = format!("{dir}/{}", entry.name);
            if entry.kind == forge_git::EntryKind::Blob
                && (entry.name.ends_with(".yml") || entry.name.ends_with(".yaml"))
                && only.is_none_or(|only| only == path)
            {
                files.push(path);
            }
        }
    }

    let mut changed: Option<Option<Vec<String>>> = None;
    let mut started = Vec::new();
    for path in files {
        let blob = git.blob(&trigger.sha, &path, MAX_WORKFLOW_SIZE).await?;
        let parsed = match blob.data {
            None => Err(anyhow::anyhow!("the workflow file is over 512 KB")),
            Some(data) => String::from_utf8(data)
                .map_err(|_| anyhow::anyhow!("the workflow file isn't UTF-8"))
                .and_then(|source| Workflow::parse(&source)),
        };
        let workflow = match parsed {
            Ok(workflow) => workflow,
            Err(error) => {
                // Like GitHub, report the broken file as a failed run.
                let id = record(state, repo, trigger, actor, &path, &path, Err(error)).await?;
                started.push(id);
                continue;
            }
        };
        if only.is_none() {
            if workflow.on.has_path_filters() && changed.is_none() {
                changed = Some(match &trigger.compare_from {
                    Some(from) => git.changed_paths(from, &trigger.sha).await.ok(),
                    None => None,
                });
            }
            let changed = changed.as_ref().and_then(|c| c.as_deref());
            let event = match trigger.event {
                "push" => Event::Push {
                    git_ref: &trigger.git_ref,
                    changed,
                },
                _ => Event::PullRequest {
                    action: trigger.action.unwrap_or(""),
                    base_branch: trigger.base_branch.as_deref().unwrap_or(""),
                    changed,
                },
            };
            if !workflow.triggered_by(&event) {
                continue;
            }
        }
        let name = workflow.name.clone().unwrap_or_else(|| path.clone());
        let jobs = plan_jobs(state, repo, trigger, &workflow);
        let id = record(
            state,
            repo,
            trigger,
            actor,
            &path,
            &name,
            jobs.map(|jobs| (workflow, jobs)),
        )
        .await?;
        started.push(id);
    }
    if !started.is_empty() {
        queued().notify_waiters();
    }
    Ok(started)
}

#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    repo: &db::Repo,
    trigger: &Trigger<'_>,
    actor: Option<&db::User>,
    path: &str,
    name: &str,
    planned: Result<(Workflow, Vec<db::NewJob>)>,
) -> Result<i64> {
    let payload = trigger.payload.to_string();
    let (title, jobs, error) = match planned {
        Ok((workflow, jobs)) => {
            let title = match &workflow.run_name {
                Some(run_name) => {
                    let ctx =
                        Context::new().with("github", base_github(state, repo, trigger, actor));
                    expr::interpolate(run_name, &ctx).unwrap_or_else(|_| trigger.title.clone())
                }
                None => trigger.title.clone(),
            };
            (title, jobs, None)
        }
        Err(error) => (
            trigger.title.clone(),
            Vec::new(),
            Some(format!("{error:#}")),
        ),
    };
    let id = db::create_run(
        &state.db,
        &db::NewRun {
            repo_id: repo.id,
            workflow_name: name,
            workflow_path: path,
            title: &title,
            event: trigger.event,
            git_ref: &trigger.git_ref,
            head_branch: trigger.head_branch.as_deref(),
            head_sha: &trigger.sha,
            pull_number: trigger.pull_number,
            actor_id: actor.map(|a| a.id),
            payload: &payload,
            error: error.as_deref(),
        },
        &jobs,
    )
    .await?;
    if let Some(run) = db::run(&state.db, id).await? {
        workflow_run_event(state, repo, &run, "requested").await;
        if run.status == "completed" {
            workflow_run_event(state, repo, &run, "completed").await;
        }
    }
    Ok(id)
}

/// The `github` context known before a run exists (for `run-name`).
fn base_github(
    state: &AppState,
    repo: &db::Repo,
    trigger: &Trigger<'_>,
    actor: Option<&db::User>,
) -> Value {
    json!({
        "event_name": trigger.event,
        "event": trigger.payload,
        "ref": trigger.git_ref,
        "ref_name": ref_name(&trigger.git_ref),
        "sha": trigger.sha,
        "repository": format!("{}/{}", repo.owner_name, repo.name),
        "repository_owner": repo.owner_name,
        "actor": actor.map(|a| a.username.clone()),
        "server_url": state.site.http_url,
        "head_ref": if trigger.event == "pull_request" { trigger.head_branch.clone() } else { None },
        "base_ref": trigger.base_branch,
    })
}

fn ref_name(git_ref: &str) -> &str {
    git_ref
        .strip_prefix("refs/heads/")
        .or_else(|| git_ref.strip_prefix("refs/tags/"))
        .unwrap_or(git_ref)
}

/// One job (per matrix combination) for each job in the workflow.
fn plan_jobs(
    state: &AppState,
    repo: &db::Repo,
    trigger: &Trigger<'_>,
    workflow: &Workflow,
) -> Result<Vec<db::NewJob>> {
    let github = base_github(state, repo, trigger, None);
    let mut jobs = Vec::new();
    for (key, job) in &workflow.jobs {
        let combos = match &job.matrix {
            Some(matrix) => matrix.expand().with_context(|| format!("in job `{key}`"))?,
            None => vec![Map::new()],
        };
        let total = combos.len();
        for (index, combo) in combos.into_iter().enumerate() {
            let matrix = Value::Object(combo.clone());
            let ctx = Context::new()
                .with("github", github.clone())
                .with("matrix", matrix.clone())
                .with(
                    "strategy",
                    json!({ "fail-fast": job.fail_fast, "job-index": index, "job-total": total }),
                )
                .with("inputs", json!({}))
                .with("vars", json!({}));
            let name = match &job.name {
                Some(name) => expr::interpolate(name, &ctx)
                    .with_context(|| format!("in the name of job `{key}`"))?,
                None if job.matrix.is_some() => {
                    let values: Vec<String> = combo
                        .values()
                        .filter(|v| !v.is_object() && !v.is_array())
                        .map(expr::to_string)
                        .collect();
                    format!("{key} ({})", values.join(", "))
                }
                None => key.clone(),
            };
            let runs_on = runs_on_labels(&job.runs_on, &ctx)
                .with_context(|| format!("in `runs-on` of job `{key}`"))?;
            if runs_on.is_empty() {
                anyhow::bail!("job `{key}` has an empty `runs-on`");
            }
            let mut step_names = vec!["Set up job".to_owned()];
            // Like GitHub, only `name:` is evaluated; `Run <command>` shows the command.
            step_names.extend(job.steps.iter().map(|step| match &step.name {
                Some(name) => expr::interpolate(name, &ctx).unwrap_or_else(|_| name.clone()),
                None => step.display_name(),
            }));
            step_names.push("Complete job".to_owned());
            let spec = json!({
                "job": job,
                "matrix": matrix,
                "strategy": { "fail-fast": job.fail_fast, "job-index": index, "job-total": total },
                "workflow_env": workflow.env,
                "workflow_defaults": workflow.defaults,
            });
            jobs.push(db::NewJob {
                key: key.clone(),
                name,
                needs: job.needs.clone(),
                runs_on,
                spec: spec.to_string(),
                step_names,
            });
        }
    }
    Ok(jobs)
}

fn runs_on_labels(runs_on: &Value, ctx: &Context) -> Result<Vec<String>> {
    let value = match runs_on {
        Value::String(s) => expr::evaluate_value(s, ctx)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| expr::interpolate(item.as_str().unwrap_or(""), ctx).map(Value::String))
                .collect::<Result<_>>()?,
        ),
        _ => Value::Null,
    };
    let labels = match value {
        Value::Array(items) => items.iter().map(expr::to_string).collect(),
        other => vec![expr::to_string(&other)],
    };
    Ok(labels
        .into_iter()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .collect())
}

/// What the job spec stored with each job holds.
pub struct StoredSpec {
    pub job: Job,
    pub matrix: Value,
    pub strategy: Value,
    pub workflow_env: Map<String, Value>,
    pub workflow_defaults: forge_ci::workflow::RunDefaults,
}

pub fn stored_spec(job: &db::CiJob) -> Result<StoredSpec> {
    let mut value: Value = serde_json::from_str(&job.spec)?;
    Ok(StoredSpec {
        job: serde_json::from_value(value["job"].take())?,
        matrix: value["matrix"].take(),
        strategy: value["strategy"].take(),
        workflow_env: serde_json::from_value(value["workflow_env"].take()).unwrap_or_default(),
        workflow_defaults: serde_json::from_value(value["workflow_defaults"].take())
            .unwrap_or_default(),
    })
}

/// The full `github` context for a job.
pub fn github_context(state: &AppState, repo: &db::Repo, run: &db::Run, job: &db::CiJob) -> Value {
    let payload: Value = serde_json::from_str(&run.payload).unwrap_or(json!({}));
    json!({
        "event_name": run.event,
        "event": payload,
        "ref": run.git_ref,
        "ref_name": ref_name(&run.git_ref),
        "ref_type": if run.git_ref.starts_with("refs/tags/") { "tag" } else { "branch" },
        "sha": run.head_sha,
        "repository": format!("{}/{}", repo.owner_name, repo.name),
        "repository_owner": repo.owner_name,
        "repositoryUrl": format!("{}/{}/{}.git", state.site.http_url, repo.owner_name, repo.name),
        "actor": run.actor,
        "triggering_actor": run.actor,
        "run_id": run.id.to_string(),
        "run_number": run.number.to_string(),
        "run_attempt": "1",
        "job": job.job_key,
        "workflow": run.workflow_name,
        "workflow_ref": format!("{}/{}/{}@{}", repo.owner_name, repo.name, run.workflow_path, run.git_ref),
        "server_url": state.site.http_url,
        "api_url": format!("{}/api/v3", state.site.http_url),
        "graphql_url": format!("{}/api/graphql", state.site.http_url),
        "head_ref": if run.event == "pull_request" { run.head_branch.clone() } else { None },
        "base_ref": payload["pull_request"]["base"]["ref"],
        "token": "",
    })
}

/// `needs` for a job: each needed job's combined result and outputs.
fn needs_context(job: &db::CiJob, all: &[db::CiJob]) -> (Value, Status) {
    let mut needs = Map::new();
    let mut status = Status::Success;
    for key in job.needs() {
        let instances: Vec<&db::CiJob> = all.iter().filter(|j| j.job_key == key).collect();
        let result = combined_result(instances.iter().map(|j| j.state()));
        let mut outputs = Map::new();
        for instance in &instances {
            if let Ok(Value::Object(o)) = serde_json::from_str(&instance.outputs) {
                outputs.extend(o);
            }
        }
        match result {
            "failure" if status == Status::Success => status = Status::Failure,
            "cancelled" => status = Status::Cancelled,
            "skipped" if status == Status::Success => status = Status::Failure,
            _ => {}
        }
        needs.insert(
            key.to_owned(),
            json!({ "result": result, "outputs": outputs }),
        );
    }
    (Value::Object(needs), status)
}

/// How a set of jobs went overall: any failure fails, then any cancellation, and only
/// skipped jobs count as skipped.
pub fn combined_result<'a>(states: impl Iterator<Item = &'a str>) -> &'static str {
    let states: Vec<&str> = states.collect();
    if states.contains(&"failure") {
        "failure"
    } else if states.contains(&"cancelled") {
        "cancelled"
    } else if !states.is_empty() && states.iter().all(|s| *s == "skipped") {
        "skipped"
    } else {
        "success"
    }
}

/// Queue (or skip) waiting jobs whose needs are done, and complete the run when every
/// job is.
pub async fn advance(state: &AppState, run_id: i64) -> Result<()> {
    let Some(run) = db::run(&state.db, run_id).await? else {
        return Ok(());
    };
    let Some(repo) = db::repo_by_id(&state.db, run.repo_id).await? else {
        return Ok(());
    };
    let mut queued_any = false;
    loop {
        let jobs = db::jobs(&state.db, run_id).await?;
        let mut changed = false;
        for job in jobs.iter().filter(|j| j.status == "waiting") {
            let ready = job.needs().all(|key| {
                jobs.iter()
                    .filter(|j| j.job_key == key)
                    .all(|j| j.status == "completed")
            });
            if !ready {
                continue;
            }
            let (needs, status) = needs_context(job, &jobs);
            let spec = stored_spec(job)?;
            let ctx = Context {
                status,
                ..Context::new()
                    .with("github", github_context(state, &repo, &run, job))
                    .with("needs", needs)
                    .with("matrix", spec.matrix.clone())
                    .with("inputs", json!({}))
                    .with("vars", json!({}))
            };
            let condition = spec.job.condition.as_deref().unwrap_or("success()");
            let (next, conclusion) = match expr::condition(condition, &ctx) {
                Ok(true) => ("queued", None),
                Ok(false) => ("completed", Some("skipped")),
                Err(error) => {
                    logs::append(
                        state,
                        job.id,
                        0,
                        &format!("The job's `if` is invalid: {error:#}\n"),
                    )
                    .await;
                    ("completed", Some("failure"))
                }
            };
            db::set_pending_job(&state.db, job.id, next, conclusion).await?;
            queued_any |= next == "queued";
            changed = true;
        }
        if !changed {
            if !jobs.is_empty() && jobs.iter().all(|j| j.status == "completed") {
                let conclusion = if run.cancelled {
                    "cancelled"
                } else {
                    combined_result(jobs.iter().map(|j| j.state()))
                };
                if db::complete_run(&state.db, run_id, conclusion).await? {
                    if let Some(run) = db::run(&state.db, run_id).await? {
                        workflow_run_event(state, &repo, &run, "completed").await;
                    }
                }
            }
            break;
        }
    }
    if queued_any {
        queued().notify_waiters();
    }
    Ok(())
}

/// A runner reported the job finished.
pub async fn job_finished(
    state: &AppState,
    job: &db::CiJob,
    conclusion: Conclusion,
    outputs: &Map<String, Value>,
) -> Result<()> {
    let spec = stored_spec(job)?;
    let mut conclusion = conclusion;
    if conclusion == Conclusion::Failure && spec.job.continue_on_error {
        logs::append(
            state,
            job.id,
            protocol::step_count(&spec.job) - 1,
            "The job failed, but has `continue-on-error`, so it counts as a success.\n",
        )
        .await;
        conclusion = Conclusion::Success;
    }
    let outputs = Value::Object(outputs.clone()).to_string();
    if !db::finish_job(&state.db, job.id, conclusion.as_str(), &outputs).await? {
        return Ok(());
    }
    if conclusion == Conclusion::Failure && spec.job.matrix.is_some() && spec.job.fail_fast {
        // fail-fast: stop the other combinations of this job.
        for sibling in db::jobs(&state.db, job.run_id).await? {
            if sibling.job_key != job.job_key || sibling.id == job.id {
                continue;
            }
            match sibling.status.as_str() {
                "waiting" | "queued" => {
                    db::set_pending_job(&state.db, sibling.id, "completed", Some("cancelled"))
                        .await?
                }
                "in_progress" => db::request_cancel_job(&state.db, sibling.id).await?,
                _ => {}
            }
        }
    }
    advance(state, job.run_id).await
}

pub async fn cancel(state: &AppState, run_id: i64) -> Result<()> {
    db::cancel_run(&state.db, run_id).await?;
    advance(state, run_id).await
}

/// Run a workflow again at the same commit, as a new run.
pub async fn rerun(
    state: &AppState,
    repo: &db::Repo,
    run: &db::Run,
    actor: &db::User,
) -> Result<Option<i64>> {
    let payload: Value = serde_json::from_str(&run.payload).unwrap_or(json!({}));
    let trigger = Trigger {
        event: &run.event,
        action: payload["action"].as_str(),
        git_ref: run.git_ref.clone(),
        sha: run.head_sha.clone(),
        head_branch: run.head_branch.clone(),
        base_branch: payload["pull_request"]["base"]["ref"]
            .as_str()
            .map(str::to_owned),
        pull_number: run.pull_number,
        title: run.title.clone(),
        payload: &payload,
        compare_from: None,
    };
    let ids = start_runs(state, repo, &trigger, Some(actor), Some(&run.workflow_path)).await?;
    Ok(ids.into_iter().next())
}

/// Fail jobs whose runner went quiet (it crashed, or lost its connection).
pub fn spawn_reaper(state: AppState) {
    tokio::spawn(async move {
        loop {
            let every = (runner_timeout() / 2).clamp(1, 30) as u64;
            tokio::time::sleep(Duration::from_secs(every)).await;
            if let Err(error) = reap(&state).await {
                tracing::warn!("checking for stuck CI jobs failed: {error:#}");
            }
        }
    });
}

async fn reap(state: &AppState) -> Result<()> {
    for id in db::stale_jobs(&state.db, runner_timeout()).await? {
        let Some(job) = db::job(&state.db, id).await? else {
            continue;
        };
        let step = db::steps(&state.db, id)
            .await?
            .into_iter()
            .find(|s| s.status == "in_progress")
            .map_or(0, |s| s.number as usize);
        logs::append(
            state,
            id,
            step,
            "\nThe runner stopped responding, so the job failed.\n",
        )
        .await;
        job_finished(state, &job, Conclusion::Failure, &Map::new()).await?;
    }
    Ok(())
}

// ---- Webhooks and the API ----

/// A run as GitHub's REST API (and `workflow_run` webhooks) show it.
pub fn run_json(state: &AppState, repo: &db::Repo, run: &db::Run) -> Value {
    let repo_url = format!("{}/{}/{}", state.site.http_url, repo.owner_name, repo.name);
    let api = format!(
        "{}/api/v3/repos/{}/{}",
        state.site.http_url, repo.owner_name, repo.name
    );
    json!({
        "id": run.id,
        "name": run.workflow_name,
        "node_id": format!("WFR_{}", run.id),
        "head_branch": run.head_branch,
        "head_sha": run.head_sha,
        "path": run.workflow_path,
        "display_title": run.title,
        "run_number": run.number,
        "run_attempt": 1,
        "event": run.event,
        "status": run.status,
        "conclusion": run.conclusion,
        "workflow_id": workflow_id(&run.workflow_path),
        "url": format!("{api}/actions/runs/{}", run.id),
        "html_url": format!("{repo_url}/actions/runs/{}", run.id),
        "jobs_url": format!("{api}/actions/runs/{}/jobs", run.id),
        "pull_requests": run.pull_number.map_or_else(Vec::new, |n| vec![json!({ "number": n })]),
        "created_at": web::render::iso8601(run.created),
        "updated_at": web::render::iso8601(run.completed.or(run.started).unwrap_or(run.created)),
        "run_started_at": web::render::iso8601(run.started.unwrap_or(run.created)),
        "actor": run.actor.as_ref().zip(run.actor_id).map(|(name, id)| crate::api::user_json(state, id, name)),
        "triggering_actor": run.actor.as_ref().zip(run.actor_id).map(|(name, id)| crate::api::user_json(state, id, name)),
        "repository": { "full_name": format!("{}/{}", repo.owner_name, repo.name) },
    })
}

/// A stable number for a workflow file, as GitHub has workflow ids.
pub fn workflow_id(path: &str) -> i64 {
    // FNV-1a, kept positive and within JavaScript's safe integers.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash & 0x1f_ffff_ffff_ffff) as i64
}

async fn workflow_run_event(state: &AppState, repo: &db::Repo, run: &db::Run, action: &str) {
    let payload = json!({
        "action": action,
        "workflow_run": run_json(state, repo, run),
        "workflow": { "id": workflow_id(&run.workflow_path), "name": run.workflow_name, "path": run.workflow_path },
        "repository": events::repository(state, repo),
    });
    if let Err(error) = events::emit(state, repo, "workflow_run", &payload).await {
        tracing::warn!("couldn't queue the workflow_run webhook: {error:#}");
    }
}

/// The checks shown for a commit: the latest run of each workflow per event, and the
/// latest status per context reported through the API.
pub struct Checks {
    pub runs: Vec<(db::Run, Vec<db::CiJob>)>,
    pub statuses: Vec<db::CommitStatus>,
}

impl Checks {
    pub async fn load(state: &AppState, repo_id: i64, sha: &str) -> Result<Self> {
        let mut runs: Vec<(db::Run, Vec<db::CiJob>)> = Vec::new();
        for run in db::runs_for_sha(&state.db, repo_id, sha).await? {
            if runs
                .iter()
                .any(|(r, _)| r.workflow_path == run.workflow_path && r.event == run.event)
            {
                continue;
            }
            let jobs = db::jobs(&state.db, run.id).await?;
            runs.push((run, jobs));
        }
        let statuses = db::latest_statuses(&state.db, repo_id, sha).await?;
        Ok(Checks { runs, statuses })
    }

    /// Each check's state: `success`, `failure`, `cancelled`, `skipped`, or pending ones
    /// (`queued`, `waiting`, `in_progress`, `pending`). Invalid workflows count as one
    /// failed check.
    pub fn states(&self) -> Vec<&str> {
        let mut states = Vec::new();
        for (run, jobs) in &self.runs {
            if jobs.is_empty() {
                states.push(run.state());
            }
            states.extend(jobs.iter().map(|j| j.state()));
        }
        for status in &self.statuses {
            states.push(match status.state.as_str() {
                "error" => "failure",
                other => other,
            });
        }
        states
    }

    /// `success`, `failure` or `pending` overall (`None` with no checks).
    pub fn summary(&self) -> Option<&'static str> {
        let states = self.states();
        if states.is_empty() {
            None
        } else if states.iter().any(|s| matches!(*s, "failure" | "cancelled")) {
            Some("failure")
        } else if states.iter().all(|s| matches!(*s, "success" | "skipped")) {
            Some("success")
        } else {
            Some("pending")
        }
    }
}
