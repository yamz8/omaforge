//! CI pages: workflow runs, their jobs, and live job logs.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Redirect},
    Json,
};
use axum_extra::extract::Form;
use serde::Deserialize;
use serde_json::json;

use super::{
    icons, page, render,
    repo::{load, Loaded, RepoContext},
    session::CsrfForm,
    Viewer, WebError, WebResult,
};
use crate::{ci, db, AppState};

const RUNS_PER_PAGE: i64 = 25;

/// How a run, job, step or check is shown: icon, CSS class and words.
#[derive(Clone, Copy)]
pub(super) struct StateView {
    pub(super) icon: &'static str,
    pub(super) class: &'static str,
    pub(super) label: &'static str,
}

pub(super) fn state_view(state: &str) -> StateView {
    let (icon, class, label) = match state {
        "success" => (icons::CHECK, "success", "Succeeded"),
        "failure" | "error" => (icons::CROSS, "failure", "Failed"),
        "cancelled" => (icons::STOPPED, "cancelled", "Cancelled"),
        "skipped" => (icons::SKIPPED, "skipped", "Skipped"),
        "in_progress" => (icons::SPINNER, "running", "In progress"),
        "waiting" => (icons::CIRCLE, "pending", "Waiting"),
        "pending" => (icons::CIRCLE, "pending", "Pending"),
        _ => (icons::CIRCLE, "pending", "Queued"),
    };
    StateView { icon, class, label }
}

/// `1m 05s`, or empty if it never started.
pub(super) fn duration(started: Option<i64>, completed: Option<i64>) -> String {
    let Some(started) = started else {
        return String::new();
    };
    let seconds = (completed.unwrap_or_else(render::now) - started).max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

pub(super) struct RunItem {
    url: String,
    title: String,
    workflow: String,
    number: i64,
    state: StateView,
    completed: bool,
    /// `push to main`, `pull request #3`, ...
    trigger: String,
    actor: String,
    ago: String,
    duration: String,
    sha: String,
    commit_url: String,
    pull_url: Option<String>,
}

fn run_item(repo: &Loaded, run: &db::Run) -> RunItem {
    let trigger = match (run.event.as_str(), run.pull_number) {
        ("pull_request", Some(n)) => format!("pull request #{n}"),
        ("push", _) => match run.git_ref.strip_prefix("refs/tags/") {
            Some(tag) => format!("push of tag {tag}"),
            None => format!(
                "push to {}",
                run.head_branch.as_deref().unwrap_or(&run.git_ref)
            ),
        },
        (event, _) => event.replace('_', " "),
    };
    RunItem {
        url: format!("{}/actions/runs/{}", repo.url, run.id),
        title: run.title.clone(),
        workflow: run.workflow_name.clone(),
        number: run.number,
        state: state_view(run.state()),
        completed: run.status == "completed",
        trigger,
        actor: run.actor.clone().unwrap_or_default(),
        ago: render::time_ago(run.created),
        duration: duration(run.started.or(Some(run.created)), run.completed),
        sha: run.head_sha.chars().take(7).collect(),
        commit_url: format!("{}/commit/{}", repo.url, run.head_sha),
        pull_url: run.pull_number.map(|n| format!("{}/pull/{n}", repo.url)),
    }
}

pub(super) struct JobItem {
    id: i64,
    url: String,
    name: String,
    state: StateView,
    duration: String,
    needs: String,
}

fn job_item(repo: &Loaded, job: &db::CiJob) -> JobItem {
    JobItem {
        id: job.id,
        url: format!("{}/actions/runs/{}/jobs/{}", repo.url, job.run_id, job.id),
        name: job.name.clone(),
        state: state_view(job.state()),
        duration: duration(job.started, job.completed),
        needs: job.needs().collect::<Vec<_>>().join(", "),
    }
}

// ---- Run list ----

struct WorkflowLink {
    name: String,
    url: String,
    active: bool,
}

#[derive(Template)]
#[template(path = "actions.html")]
struct ActionsPage {
    viewer: Viewer,
    repo: RepoContext,
    runs: Vec<RunItem>,
    workflows: Vec<WorkflowLink>,
    all_url: String,
    filtered: bool,
    total: i64,
    newer: Option<String>,
    older: Option<String>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    workflow: Option<String>,
    page: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let workflow = query.workflow.filter(|w| !w.is_empty());
    let page_number = query.page.unwrap_or(1).max(1);
    let total = db::run_count(&state.db, repo.repo.id, workflow.as_deref()).await?;
    let runs = db::runs(
        &state.db,
        repo.repo.id,
        workflow.as_deref(),
        RUNS_PER_PAGE,
        (page_number - 1) * RUNS_PER_PAGE,
    )
    .await?;
    let base = format!("{}/actions", repo.url);
    let filter = workflow
        .as_deref()
        .map(|w| format!("workflow={}&", super::form_urlencode(w)))
        .unwrap_or_default();
    let workflows = db::workflows(&state.db, repo.repo.id)
        .await?
        .into_iter()
        .map(|(path, name)| WorkflowLink {
            url: format!("{base}?workflow={}", super::form_urlencode(&path)),
            active: workflow.as_deref() == Some(path.as_str()),
            name,
        })
        .collect();
    page(ActionsPage {
        runs: runs.iter().map(|r| run_item(&repo, r)).collect(),
        workflows,
        all_url: base.clone(),
        filtered: workflow.is_some(),
        total,
        newer: (page_number > 1).then(|| format!("{base}?{filter}page={}", page_number - 1)),
        older: (page_number * RUNS_PER_PAGE < total)
            .then(|| format!("{base}?{filter}page={}", page_number + 1)),
        repo: repo.context(&state, "actions"),
        viewer: repo.viewer,
    })
}

// ---- One run ----

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    viewer: Viewer,
    repo: RepoContext,
    run: RunItem,
    error: Option<String>,
    jobs: Vec<JobItem>,
    can_write: bool,
    workflow_url: String,
}

async fn load_run(state: &AppState, repo: &Loaded, id: i64) -> WebResult<db::Run> {
    db::repo_run(&state.db, repo.repo.id, id)
        .await?
        .ok_or(WebError::NotFound)
}

pub async fn show(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let run = load_run(&state, &repo, id).await?;
    let jobs = db::jobs(&state.db, run.id).await?;
    page(RunPage {
        workflow_url: format!(
            "{}/blob/{}/{}",
            repo.url,
            run.head_sha,
            super::repo::encode_path(&run.workflow_path)
        ),
        run: run_item(&repo, &run),
        error: run.error.clone(),
        jobs: jobs.iter().map(|j| job_item(&repo, j)).collect(),
        can_write: repo.can_write(),
        repo: repo.context(&state, "actions"),
        viewer: repo.viewer,
    })
}

async fn writable_run(
    state: &AppState,
    viewer: Viewer,
    owner: &str,
    name: &str,
    id: i64,
    csrf: &str,
) -> WebResult<(Loaded, db::Run, db::User)> {
    let repo = load(state, viewer, owner, name).await?;
    let user = repo
        .viewer
        .require(&format!("{}/actions/runs/{id}", repo.url))?
        .clone();
    repo.viewer.check_csrf(csrf)?;
    if !repo.can_write() {
        return Err(WebError::Forbidden);
    }
    let run = load_run(state, &repo, id).await?;
    Ok((repo, run, user))
}

pub async fn cancel(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let (repo, run, _) = writable_run(&state, viewer, &owner, &name, id, form.csrf()).await?;
    ci::cancel(&state, run.id).await?;
    Ok(Redirect::to(&format!("{}/actions/runs/{}", repo.url, run.id)).into_response())
}

pub async fn rerun(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let (repo, run, user) = writable_run(&state, viewer, &owner, &name, id, form.csrf()).await?;
    let new = ci::rerun(&state, &repo.repo, &run, &user).await?;
    // The workflow file may be gone from that commit; then there's nothing to show.
    let target = new.unwrap_or(run.id);
    Ok(Redirect::to(&format!("{}/actions/runs/{target}", repo.url)).into_response())
}

// ---- One job ----

pub(super) struct LogLine {
    class: &'static str,
    text: String,
}

/// Lines of a step's log, with `##[group]` / `##[error]` markers turned into styles.
fn log_lines(text: &str) -> Vec<LogLine> {
    text.lines()
        .filter_map(|line| {
            let (class, text) = if let Some(rest) = line.strip_prefix("##[group]") {
                ("group", rest)
            } else if line.starts_with("##[endgroup]") {
                return None;
            } else if let Some(rest) = line.strip_prefix("##[error]") {
                ("error", rest)
            } else if let Some(rest) = line.strip_prefix("##[warning]") {
                ("warning", rest)
            } else {
                ("", line)
            };
            Some(LogLine {
                class,
                text: text.to_owned(),
            })
        })
        .collect()
}

struct StepView {
    number: i64,
    name: String,
    state: StateView,
    duration: String,
    lines: Vec<LogLine>,
    /// Bytes of log shown, where live updates continue.
    offset: u64,
    open: bool,
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobPage {
    viewer: Viewer,
    repo: RepoContext,
    run: RunItem,
    jobs: Vec<JobItem>,
    job: JobItem,
    runner: Option<String>,
    labels: String,
    steps: Vec<StepView>,
    live: bool,
    live_url: String,
    log_url: String,
    can_write: bool,
}

async fn load_job(
    state: &AppState,
    repo: &Loaded,
    run_id: i64,
    job_id: i64,
) -> WebResult<(db::Run, db::CiJob)> {
    let run = load_run(state, repo, run_id).await?;
    let job = db::job(&state.db, job_id)
        .await?
        .filter(|j| j.run_id == run.id)
        .ok_or(WebError::NotFound)?;
    Ok((run, job))
}

pub async fn job(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, run_id, job_id)): Path<(String, String, i64, i64)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (run, job) = load_job(&state, &repo, run_id, job_id).await?;
    let jobs = db::jobs(&state.db, run.id).await?;
    let mut steps = Vec::new();
    for step in db::steps(&state.db, job.id).await? {
        let (text, offset) = ci::logs::read(&state, job.id, step.number as usize, 0).await?;
        steps.push(StepView {
            open: matches!(step.state(), "failure" | "in_progress"),
            number: step.number,
            state: state_view(step.state()),
            duration: duration(step.started, step.completed),
            lines: log_lines(&text),
            offset,
            name: step.name,
        });
    }
    let item = job_item(&repo, &job);
    page(JobPage {
        live: job.status != "completed",
        live_url: format!("{}/live", item.url),
        log_url: format!("{}/logs", item.url),
        runner: job.runner.clone(),
        labels: job.labels().collect::<Vec<_>>().join(", "),
        run: run_item(&repo, &run),
        jobs: jobs.iter().map(|j| job_item(&repo, j)).collect(),
        job: item,
        steps,
        can_write: repo.can_write(),
        repo: repo.context(&state, "actions"),
        viewer: repo.viewer,
    })
}

#[derive(Deserialize)]
pub struct LiveQuery {
    /// Comma-separated log offsets, by step number.
    #[serde(default)]
    o: String,
}

/// New log output and step states since the offsets the page has, for live updates.
pub async fn live(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, run_id, job_id)): Path<(String, String, i64, i64)>,
    Query(query): Query<LiveQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (_, job) = load_job(&state, &repo, run_id, job_id).await?;
    let offsets: Vec<u64> = query.o.split(',').map(|o| o.parse().unwrap_or(0)).collect();
    let mut steps = Vec::new();
    for step in db::steps(&state.db, job.id).await? {
        let n = step.number as usize;
        let (text, offset) =
            ci::logs::read(&state, job.id, n, offsets.get(n).copied().unwrap_or(0)).await?;
        let view = state_view(step.state());
        steps.push(json!({
            "n": n,
            "class": view.class,
            "label": view.label,
            "icon": view.icon,
            "duration": duration(step.started, step.completed),
            "lines": log_lines(&text).into_iter().map(|l| json!([l.class, l.text])).collect::<Vec<_>>(),
            "offset": offset,
        }));
    }
    let view = state_view(job.state());
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "done": job.status == "completed",
            "class": view.class,
            "label": view.label,
            "icon": view.icon,
            "steps": steps,
        })),
    )
        .into_response())
}

pub async fn logs(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, run_id, job_id)): Path<(String, String, i64, i64)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (_, job) = load_job(&state, &repo, run_id, job_id).await?;
    let steps = db::steps(&state.db, job.id).await?;
    let text = ci::logs::full(&state, job.id, &steps).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        text,
    )
        .into_response())
}

// ---- Checks on commits and pull requests ----

pub(super) struct CheckItem {
    pub(super) name: String,
    pub(super) context: String,
    pub(super) state: StateView,
    pub(super) description: String,
    pub(super) url: String,
}

/// Checks for a commit, for the pull request page and commit pages.
pub(super) struct ChecksView {
    pub(super) items: Vec<CheckItem>,
    /// `success`, `failure` or `pending`.
    pub(super) summary: &'static str,
    pub(super) headline: String,
}

pub(super) async fn checks_view(
    state: &AppState,
    repo: &Loaded,
    sha: &str,
) -> WebResult<Option<ChecksView>> {
    let checks = ci::Checks::load(state, repo.repo.id, sha).await?;
    let Some(summary) = checks.summary() else {
        return Ok(None);
    };
    let mut items = Vec::new();
    for (run, jobs) in &checks.runs {
        let run_url = format!("{}/actions/runs/{}", repo.url, run.id);
        if jobs.is_empty() {
            items.push(CheckItem {
                name: run.workflow_name.clone(),
                context: format!("({})", run.event.replace('_', " ")),
                state: state_view(run.state()),
                description: run
                    .error
                    .clone()
                    .map(|_| "Invalid workflow file".to_owned())
                    .unwrap_or_default(),
                url: run_url.clone(),
            });
        }
        for job in jobs {
            let view = state_view(job.state());
            items.push(CheckItem {
                name: format!("{} / {}", run.workflow_name, job.name),
                context: format!("({})", run.event.replace('_', " ")),
                description: match job.status.as_str() {
                    "completed" => format!(
                        "{} after {}",
                        view.label,
                        duration(job.started, job.completed)
                    )
                    .trim_end_matches(" after ")
                    .to_owned(),
                    _ => view.label.to_owned(),
                },
                state: view,
                url: format!("{run_url}/jobs/{}", job.id),
            });
        }
    }
    for status in &checks.statuses {
        items.push(CheckItem {
            name: status.context.clone(),
            context: String::new(),
            state: state_view(&status.state),
            description: status.description.clone(),
            url: safe_url(&status.target_url),
        });
    }
    // Problems first, as on GitHub.
    let rank = |class: &str| match class {
        "failure" => 0,
        "cancelled" => 1,
        "running" => 2,
        "pending" => 3,
        "success" => 4,
        _ => 5,
    };
    items.sort_by_key(|i| rank(i.state.class));
    let count = |class: &str| items.iter().filter(|i| i.state.class == class).count();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let headline = match summary {
        "success" => "All checks have passed".to_owned(),
        "failure" => {
            let failed = count("failure") + count("cancelled");
            format!("{} failed", plural(failed, "check"))
        }
        _ => {
            let running = count("running") + count("pending");
            format!("{} still running", plural(running, "check"))
        }
    };
    Ok(Some(ChecksView {
        items,
        summary,
        headline,
    }))
}

/// A commit's overall CI result, as a small icon in commit lists.
pub(super) struct CommitChecks {
    pub(super) class: &'static str,
    pub(super) icon: &'static str,
    pub(super) title: String,
    pub(super) url: String,
}

pub(super) async fn add_checks(
    state: &AppState,
    repo: &Loaded,
    view: Option<&mut super::repo::CommitView>,
) -> WebResult<()> {
    let Some(view) = view else { return Ok(()) };
    let checks = ci::Checks::load(state, repo.repo.id, &view.id).await?;
    let Some(summary) = checks.summary() else {
        return Ok(());
    };
    let states = checks.states();
    let passed = states
        .iter()
        .filter(|s| matches!(**s, "success" | "skipped"))
        .count();
    let shown = state_view(match summary {
        "pending" => "in_progress",
        other => other,
    });
    let url = match checks.runs.first() {
        Some((run, _)) if checks.runs.len() == 1 => format!("{}/actions/runs/{}", repo.url, run.id),
        _ => format!("{}/actions", repo.url),
    };
    view.checks = Some(CommitChecks {
        class: shown.class,
        icon: shown.icon,
        title: format!("{passed} of {} checks passed", states.len()),
        url,
    });
    Ok(())
}

/// Only http(s) links from statuses reported through the API.
fn safe_url(url: &str) -> String {
    if url.starts_with("https://") || url.starts_with("http://") {
        url.to_owned()
    } else {
        String::new()
    }
}
