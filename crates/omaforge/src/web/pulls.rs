//! Pull requests: comparing branches, opening, discussing, reviewing and merging.

use std::collections::BTreeMap;

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect},
    Form,
};
use forge_git::{diff::FileStatus, Mergeability};
use serde::Deserialize;

use super::{
    conversation::{self, Branches, Mentions, Timeline, TimelineEntry},
    diff::{self, path_hash, FileView, InlineComment, LineCommentForm},
    icons, page, render,
    repo::{load, CommitView, Loaded, RepoContext},
    Viewer, WebError, WebResult,
};
use crate::{actions, db, events, hooks::RefUpdate, AppState};

/// Commits shown when comparing branches or on a pull request.
const COMMIT_LIMIT: usize = 250;

// ---- Shared pieces ----

/// A pull request with its branches resolved to commits.
struct Live {
    pull: db::Pull,
    base_sha: String,
    head_sha: String,
    base_exists: bool,
    head_exists: bool,
}

/// Look up a pull request and, while it's open, follow its branches to their current
/// tips (and remember them, so the pull request survives its branch being deleted).
async fn live_pull(state: &AppState, repo: &Loaded, number: i64) -> WebResult<Live> {
    let pull = match db::pull(&state.db, repo.repo.id, number).await? {
        Some(pull) => pull,
        None if db::number_kind(&state.db, repo.repo.id, number).await? == Some(false) => {
            return Err(WebError::Redirect(format!("{}/issues/{number}", repo.url)));
        }
        None => return Err(WebError::NotFound),
    };
    let tips = actions::pull_tips(state, &repo.git, &pull).await?;
    let live = Live {
        base_sha: tips.base_sha,
        head_sha: tips.head_sha,
        base_exists: tips.base_exists,
        head_exists: tips.head_exists,
        pull,
    };
    Ok(live)
}

struct PullContext {
    number: i64,
    title: String,
    /// `open`, `merged` or `closed`, for styling.
    state: &'static str,
    state_label: &'static str,
    state_icon: &'static str,
    author: String,
    created_ago: String,
    base_branch: String,
    head_branch: String,
    base_url: String,
    head_url: Option<String>,
    url: String,
    commit_count: usize,
    file_count: usize,
    /// `conversation`, `commits` or `files`.
    tab: &'static str,
}

fn pull_context(
    repo: &Loaded,
    live: &Live,
    commit_count: usize,
    file_count: usize,
    tab: &'static str,
) -> PullContext {
    let pull = &live.pull;
    let (state, state_label, state_icon) = if pull.is_merged() {
        ("merged", "Merged", icons::MERGED)
    } else if pull.is_open() {
        ("open", "Open", icons::PULL)
    } else {
        ("closed", "Closed", icons::CLOSED_PULL)
    };
    PullContext {
        number: pull.number,
        title: pull.title.clone(),
        state,
        state_label,
        state_icon,
        author: pull.author.clone(),
        created_ago: render::time_ago(pull.created),
        base_branch: pull.base_branch.clone(),
        head_branch: pull.head_branch.clone(),
        base_url: repo.view_url("tree", &pull.base_branch, ""),
        head_url: live
            .head_exists
            .then(|| repo.view_url("tree", &pull.head_branch, "")),
        url: format!("{}/pull/{}", repo.url, pull.number),
        commit_count,
        file_count,
        tab,
    }
}

/// Whether the viewer may close, reopen or merge: the author can close their own pull
/// request; merging needs write access.
fn can_write(repo: &Loaded) -> bool {
    repo.can_write()
}

fn is_author(repo: &Loaded, pull: &db::Pull) -> bool {
    repo.viewer
        .user
        .as_ref()
        .is_some_and(|u| u.id == pull.author_id)
}

// ---- List ----

struct PullListItem {
    number: i64,
    title: String,
    url: String,
    state: &'static str,
    icon: &'static str,
    author: String,
    created_ago: String,
    head_branch: String,
    comment_count: i64,
}

#[derive(Template)]
#[template(path = "pulls.html")]
struct PullsPage {
    viewer: Viewer,
    repo: RepoContext,
    showing_open: bool,
    open_count: i64,
    closed_count: i64,
    pulls: Vec<PullListItem>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let showing_open = query.state.as_deref() != Some("closed");
    let (open_count, closed_count) = db::pull_counts(&state.db, repo.repo.id).await?;
    let pulls = db::pulls(
        &state.db,
        repo.repo.id,
        if showing_open { "open" } else { "closed" },
    )
    .await?
    .into_iter()
    .map(|p| {
        let (state, icon) = if p.is_merged() {
            ("merged", icons::MERGED)
        } else if p.is_open() {
            ("open", icons::PULL)
        } else {
            ("closed", icons::CLOSED_PULL)
        };
        PullListItem {
            url: format!("{}/pull/{}", repo.url, p.number),
            number: p.number,
            title: p.title,
            state,
            icon,
            author: p.author,
            created_ago: render::time_ago(p.created),
            head_branch: p.head_branch,
            comment_count: p.comment_count,
        }
    })
    .collect();
    page(PullsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "pulls"),
        showing_open,
        open_count,
        closed_count,
        pulls,
    })
}

// ---- Compare and open ----

#[derive(Template)]
#[template(path = "compare.html")]
struct ComparePage {
    viewer: Viewer,
    repo: RepoContext,
    branches: Vec<String>,
    base: String,
    head: String,
    /// What comparing the two branches found, if both were chosen.
    message: Option<String>,
    existing: Option<(i64, String)>,
    mergeable: Option<bool>,
    commits: Vec<CommitView>,
    files: Vec<FileView>,
    additions: usize,
    deletions: usize,
    /// Prefilled pull request form, when one can be opened.
    can_open: bool,
    title: String,
    body: String,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct CompareQuery {
    base: Option<String>,
    head: Option<String>,
}

pub async fn compare(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<CompareQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let base = match query.base {
        Some(base) => base,
        None => repo.git.default_branch().await?.unwrap_or_default(),
    };
    compare_page(
        &state,
        &repo,
        base,
        query.head.unwrap_or_default(),
        None,
        None,
    )
    .await
}

async fn compare_page(
    state: &AppState,
    repo: &Loaded,
    base: String,
    head: String,
    form: Option<(String, String)>,
    error: Option<String>,
) -> WebResult {
    let branches: Vec<String> = repo
        .git
        .branches()
        .await?
        .into_iter()
        .map(|b| b.name)
        .collect();
    let mut view = ComparePage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, "pulls"),
        branches,
        base: base.clone(),
        head: head.clone(),
        message: None,
        existing: None,
        mergeable: None,
        commits: Vec::new(),
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        can_open: false,
        title: String::new(),
        body: String::new(),
        error,
    };
    if base.is_empty() || head.is_empty() {
        view.message =
            Some("Choose two branches to see what changed and open a pull request.".into());
        return page(view);
    }
    let (Some(base_sha), Some(head_sha)) = (
        view.branches.contains(&base).then_some(()).and(
            repo.git
                .resolve_commit(&format!("refs/heads/{base}"))
                .await?,
        ),
        view.branches.contains(&head).then_some(()).and(
            repo.git
                .resolve_commit(&format!("refs/heads/{head}"))
                .await?,
        ),
    ) else {
        view.message = Some("One of those branches doesn't exist.".into());
        return page(view);
    };
    if base == head {
        view.message = Some("Choose two different branches.".into());
        return page(view);
    }
    if let Some(number) = db::open_pull_for(&state.db, repo.repo.id, &base, &head).await? {
        view.existing = Some((number, format!("{}/pull/{number}", repo.url)));
    }
    let commits = repo
        .git
        .compare_log(&base_sha, &head_sha, COMMIT_LIMIT)
        .await?;
    if commits.is_empty() {
        view.message = Some(format!("{head} has nothing that isn't already in {base}."));
        return page(view);
    }
    view.mergeable = Some(matches!(
        repo.git.mergeability(&base_sha, &head_sha).await?,
        Mergeability::Clean
    ));
    let diff = repo.git.compare_diff(&base_sha, &head_sha).await?;
    view.files = diff::file_views(&diff, |f| blob_url(repo, f, &base_sha, &head_sha));
    view.additions = view.files.iter().map(|f| f.additions).sum();
    view.deletions = view.files.iter().map(|f| f.deletions).sum();
    let (title, body) = form.unwrap_or_else(|| default_description(&head, &commits));
    view.title = title;
    view.body = body;
    view.can_open = repo.viewer.user.is_some() && view.existing.is_none();
    view.commits = commits.iter().map(|c| repo.commit_view(c)).collect();
    page(view)
}

/// A single commit's message, or the branch name for several.
fn default_description(head: &str, commits: &[forge_git::Commit]) -> (String, String) {
    if let [commit] = commits {
        return (commit.subject.clone(), commit.body.clone());
    }
    let name = head
        .rsplit('/')
        .next()
        .unwrap_or(head)
        .replace(['-', '_'], " ");
    let mut chars = name.chars();
    let title = chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default();
    (title, String::new())
}

fn blob_url(repo: &Loaded, file: &forge_git::diff::FileDiff, base: &str, head: &str) -> String {
    if file.status == FileStatus::Deleted {
        repo.view_url("blob", base, file.path())
    } else {
        repo.view_url("blob", head, file.path())
    }
}

#[derive(Deserialize)]
pub struct OpenForm {
    csrf: String,
    base: String,
    head: String,
    title: String,
    #[serde(default)]
    body: String,
}

pub async fn open(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<OpenForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let user = repo
        .viewer
        .require(&format!("{}/compare", repo.url))?
        .clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let title = form.title.trim().to_owned();
    let body = form.body.trim().to_owned();
    let retry = |error: &str| (Some((title.clone(), body.clone())), Some(error.to_owned()));

    let (form_values, error) = if title.is_empty() {
        retry("Give the pull request a title.")
    } else {
        match actions::check_new_pull(&state, &repo.repo, &repo.git, &form.base, &form.head).await?
        {
            Err(reason) => retry(reason),
            Ok((base_sha, head_sha)) => {
                let number = db::create_pull(
                    &state.db,
                    &db::NewPull {
                        repo_id: repo.repo.id,
                        author_id: user.id,
                        title: &title,
                        body: &body,
                        base_branch: &form.base,
                        head_branch: &form.head,
                        base_sha: &base_sha,
                        head_sha: &head_sha,
                    },
                )
                .await?;
                tracing::info!(repo = %repo.url, number, user = %user.username, "opened pull request");
                events::pull_request(&state, &repo.repo, number, "opened", &user).await;
                return Ok(Redirect::to(&format!("{}/pull/{number}", repo.url)).into_response());
            }
        }
    };
    compare_page(&state, &repo, form.base, form.head, form_values, error).await
}

// ---- Conversation ----

struct MergeBox {
    /// `merged`, `closed`, `missing`, `uptodate`, `conflicts`, `blocked` or `clean`.
    status: &'static str,
    conflicts: Vec<String>,
    /// Why branch protection stops the merge.
    blocked_by: Option<String>,
    can_merge: bool,
    merged_by: String,
    merge_url: String,
    merge_short: String,
    /// The head branch can be deleted (merged, still at the merged commit).
    can_delete_branch: bool,
    /// Latest review from each reviewer, e.g. ("bob", "approved").
    reviews: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "pull.html")]
struct PullPage {
    viewer: Viewer,
    repo: RepoContext,
    pull: PullContext,
    description: String,
    timeline: Vec<TimelineEntry>,
    merge: MergeBox,
    can_close: bool,
    can_reopen: bool,
    can_review: bool,
    flash: Option<String>,
}

pub async fn conversation(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let live = live_pull(&state, &repo, number).await?;
    conversation_page(&state, &repo, live, None).await
}

async fn conversation_page(
    state: &AppState,
    repo: &Loaded,
    live: Live,
    flash: Option<String>,
) -> WebResult {
    let pull = &live.pull;
    let commits = repo
        .git
        .compare_log(&live.base_sha, &live.head_sha, COMMIT_LIMIT)
        .await
        .unwrap_or_default();
    let files = repo
        .git
        .compare_diff(&live.base_sha, &live.head_sha)
        .await
        .map(|d| d.len())
        .unwrap_or(0);
    let pull_url = format!("{}/pull/{}", repo.url, pull.number);

    let mentions = Mentions::load(state, repo).await?;
    let mut timeline = Timeline::default();
    for c in db::comments(&state.db, pull.issue_id).await? {
        let context = match (&c.path, c.line, &c.side) {
            (Some(path), Some(line), Some(side)) => Some((
                format!("{path}:{line}"),
                format!("{pull_url}/files#diff-{}", diff_anchor(path, side, line)),
            )),
            _ => None,
        };
        timeline.push(c.created, conversation::comment(c, &mentions, context));
    }
    let reviews = db::reviews(&state.db, pull.issue_id).await?;
    let mut latest: BTreeMap<String, String> = BTreeMap::new();
    for r in &reviews {
        if r.state != "commented" {
            latest.insert(r.author.clone(), r.state.clone());
        }
        let mut text = match r.state.as_str() {
            "approved" => "approved these changes",
            "changes_requested" => "requested changes",
            _ => "reviewed",
        }
        .to_owned();
        if r.commit_sha != live.head_sha {
            text.push_str(" on an earlier version");
        }
        timeline.push(
            r.created,
            TimelineEntry {
                kind: "review",
                id: r.id,
                who: r.author.clone(),
                text,
                html: mentions.render(&r.body),
                time_ago: render::time_ago(r.created),
                tone: r.state.clone(),
                context: None,
            },
        );
    }
    let branches = Branches {
        base: &pull.base_branch,
        head: &pull.head_branch,
    };
    for e in db::events(&state.db, pull.issue_id).await? {
        timeline.push(
            e.created,
            conversation::event(e, &repo.url, Some(&branches)),
        );
    }

    let writer = can_write(repo);
    let (status, conflicts) = if pull.is_merged() {
        ("merged", Vec::new())
    } else if !pull.is_open() {
        ("closed", Vec::new())
    } else if !live.head_exists || !live.base_exists {
        ("missing", Vec::new())
    } else {
        match repo
            .git
            .mergeability(&live.base_sha, &live.head_sha)
            .await?
        {
            Mergeability::Clean => ("clean", Vec::new()),
            Mergeability::Conflicts(files) => ("conflicts", files),
            Mergeability::UpToDate => ("uptodate", Vec::new()),
        }
    };
    let blocked_by = if status == "clean" {
        actions::merge_blocker(state, &repo.repo, pull).await?
    } else {
        None
    };
    let status = if blocked_by.is_some() {
        "blocked"
    } else {
        status
    };
    let merge_sha = pull.merge_sha.clone().unwrap_or_default();
    let head_now = repo
        .git
        .resolve_commit(&format!("refs/heads/{}", pull.head_branch))
        .await?;
    let merge = MergeBox {
        status,
        conflicts,
        blocked_by,
        can_merge: writer && status == "clean",
        merged_by: pull.merged_by.clone().unwrap_or_default(),
        merge_url: format!("{}/commit/{merge_sha}", repo.url),
        merge_short: merge_sha[..7.min(merge_sha.len())].to_owned(),
        can_delete_branch: writer
            && pull.is_merged()
            && head_now.as_deref() == Some(live.head_sha.as_str())
            && pull.head_branch != pull.base_branch,
        reviews: latest.into_iter().collect(),
    };
    let author = is_author(repo, pull);
    page(PullPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, "pulls"),
        pull: pull_context(repo, &live, commits.len(), files, "conversation"),
        description: mentions.render(&pull.body),
        timeline: timeline.finish(),
        merge,
        can_close: pull.is_open() && (writer || author),
        can_reopen: !pull.is_open()
            && !pull.is_merged()
            && (writer || author)
            && live.head_exists
            && live.base_exists,
        can_review: repo.viewer.user.is_some() && !author && pull.is_open(),
        flash,
    })
}

/// The id of a diff row for a review comment, matching `diff_file.html`.
fn diff_anchor(path: &str, side: &str, line: i64) -> String {
    format!("{}-{side}{line}", path_hash(path))
}

// ---- Commits and files ----

#[derive(Template)]
#[template(path = "pull_commits.html")]
struct PullCommitsPage {
    viewer: Viewer,
    repo: RepoContext,
    pull: PullContext,
    commits: Vec<CommitView>,
}

pub async fn commits(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let live = live_pull(&state, &repo, number).await?;
    let commits = repo
        .git
        .compare_log(&live.base_sha, &live.head_sha, COMMIT_LIMIT)
        .await?;
    let files = repo
        .git
        .compare_diff(&live.base_sha, &live.head_sha)
        .await?
        .len();
    page(PullCommitsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "pulls"),
        pull: pull_context(&repo, &live, commits.len(), files, "commits"),
        commits: commits.iter().map(|c| repo.commit_view(c)).collect(),
    })
}

#[derive(Template)]
#[template(path = "pull_files.html")]
struct PullFilesPage {
    viewer: Viewer,
    repo: RepoContext,
    pull: PullContext,
    files: Vec<FileView>,
    additions: usize,
    deletions: usize,
    can_review: bool,
}

#[derive(Deserialize)]
pub struct FilesQuery {
    /// Open a comment form on this line: `path`, `side` (`old`/`new`) and `line`.
    path: Option<String>,
    side: Option<String>,
    line: Option<u32>,
}

pub async fn files(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(query): Query<FilesQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let live = live_pull(&state, &repo, number).await?;
    let commits = repo
        .git
        .compare_log(&live.base_sha, &live.head_sha, COMMIT_LIMIT)
        .await?;
    let diff = repo
        .git
        .compare_diff(&live.base_sha, &live.head_sha)
        .await?;
    let mut files = diff::file_views(&diff, |f| {
        blob_url(&repo, f, &live.base_sha, &live.head_sha)
    });
    let files_url = format!("{}/pull/{}/files", repo.url, live.pull.number);
    let signed_in = repo.viewer.user.is_some();

    // Put existing comments under their lines, or at the top of the file if the line
    // isn't in the diff any more.
    let mut comments = db::comments(&state.db, live.pull.issue_id).await?;
    comments.retain(|c| c.path.is_some());
    for file in &mut files {
        let hash = path_hash(&file.path);
        for row in &mut file.rows {
            if let Some((side, line)) = row.anchor {
                if signed_in && live.pull.is_open() {
                    row.comment_url = Some(format!(
                        "{files_url}?path={}&side={side}&line={line}#diff-{hash}-{side}{line}",
                        super::form_urlencode(&file.path)
                    ));
                }
                let open_form = query.path.as_deref() == Some(file.path.as_str())
                    && query.side.as_deref() == Some(side)
                    && query.line == Some(line);
                if open_form && signed_in && live.pull.is_open() {
                    row.form = Some(LineCommentForm {
                        action: format!("{}/pull/{}/comment", repo.url, live.pull.number),
                        csrf: repo.viewer.csrf.clone(),
                        path: file.path.clone(),
                        side,
                        line,
                        commit: live.head_sha.clone(),
                        cancel_url: format!("{files_url}#diff-{hash}-{side}{line}"),
                    });
                }
            }
        }
        for c in comments
            .iter()
            .filter(|c| c.path.as_deref() == Some(file.path.as_str()))
        {
            let view = InlineComment {
                id: c.id,
                author: c.author.clone(),
                html: render::markdown(&c.body, None),
                time_ago: render::time_ago(c.created),
                outdated: c.commit_sha.as_deref() != Some(live.head_sha.as_str()),
            };
            let target = file.rows.iter_mut().find(|row| {
                row.anchor.is_some_and(|(side, line)| {
                    Some(side) == c.side.as_deref() && Some(i64::from(line)) == c.line
                })
            });
            match target {
                Some(row) => row.comments.push(view),
                None => file.outdated.push(view),
            }
        }
    }
    let additions = files.iter().map(|f| f.additions).sum();
    let deletions = files.iter().map(|f| f.deletions).sum();
    page(PullFilesPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "pulls"),
        pull: pull_context(&repo, &live, commits.len(), files.len(), "files"),
        can_review: signed_in && !is_author(&repo, &live.pull) && live.pull.is_open(),
        additions,
        deletions,
        files,
    })
}

// ---- Actions ----

#[derive(Deserialize)]
pub struct CommentForm {
    csrf: String,
    body: String,
    path: Option<String>,
    side: Option<String>,
    line: Option<i64>,
    commit: Option<String>,
}

pub async fn comment(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<CommentForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let pull_url = format!("{}/pull/{number}", repo.url);
    let user = repo.viewer.require(&pull_url)?.clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let live = live_pull(&state, &repo, number).await?;
    let body = form.body.trim();
    if body.is_empty() {
        return Ok(Redirect::to(&pull_url).into_response());
    }
    let anchor = match (&form.path, &form.side, form.line, &form.commit) {
        (Some(path), Some(side), Some(line), Some(commit))
            if matches!(side.as_str(), "old" | "new") && line > 0 =>
        {
            Some(db::LineAnchor {
                path,
                line,
                side,
                commit_sha: commit,
            })
        }
        _ => None,
    };
    db::add_comment(
        &state.db,
        live.pull.issue_id,
        user.id,
        body,
        anchor.as_ref(),
    )
    .await?;
    // Line comments are part of reviews; conversation comments are `issue_comment`s.
    if anchor.is_none() {
        events::comment(&state, &repo.repo, number, body, &user).await;
    }
    let back = match &anchor {
        Some(a) => format!(
            "{pull_url}/files#diff-{}",
            diff_anchor(a.path, a.side, a.line)
        ),
        None => format!("{pull_url}#bottom"),
    };
    Ok(Redirect::to(&back).into_response())
}

#[derive(Deserialize)]
pub struct ReviewForm {
    csrf: String,
    /// `approved`, `changes_requested` or `commented`.
    state: String,
    #[serde(default)]
    body: String,
}

pub async fn review(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<ReviewForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let pull_url = format!("{}/pull/{number}", repo.url);
    let user = repo.viewer.require(&pull_url)?.clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let live = live_pull(&state, &repo, number).await?;
    let body = form.body.trim();
    let error = if !matches!(
        form.state.as_str(),
        "approved" | "changes_requested" | "commented"
    ) {
        Some("Choose a review type.")
    } else if is_author(&repo, &live.pull) {
        Some("You can't review your own pull request.")
    } else if !live.pull.is_open() {
        Some("This pull request is closed.")
    } else if form.state != "approved" && body.is_empty() {
        Some("Say what should change, or leave a comment.")
    } else {
        None
    };
    if let Some(error) = error {
        return conversation_page(&state, &repo, live, Some(error.into())).await;
    }
    db::add_review(
        &state.db,
        live.pull.issue_id,
        user.id,
        &form.state,
        body,
        &live.head_sha,
    )
    .await?;
    events::review(&state, &repo.repo, number, &form.state, body, &user).await;
    Ok(Redirect::to(&format!("{pull_url}#bottom")).into_response())
}

#[derive(Deserialize)]
pub struct MergeForm {
    csrf: String,
    /// `merge`, `squash` or `rebase`.
    method: String,
}

pub async fn merge(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<MergeForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let pull_url = format!("{}/pull/{number}", repo.url);
    let user = repo.viewer.require(&pull_url)?.clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let live = live_pull(&state, &repo, number).await?;
    if !can_write(&repo) {
        return Err(WebError::Forbidden);
    }
    let Some(method) = actions::parse_merge_method(&form.method) else {
        return conversation_page(&state, &repo, live, Some("Choose how to merge.".into())).await;
    };
    match actions::merge_pull(&state, &repo.repo, &repo.git, &live.pull, &user, method).await? {
        Ok(_) => Ok(Redirect::to(&pull_url).into_response()),
        Err(reason) => conversation_page(&state, &repo, live, Some(reason)).await,
    }
}

#[derive(Deserialize)]
pub struct StateForm {
    csrf: String,
    /// `close` or `reopen`.
    action: String,
    /// A comment posted along with closing or reopening.
    #[serde(default)]
    body: String,
}

pub async fn set_state(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<StateForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let pull_url = format!("{}/pull/{number}", repo.url);
    let user = repo.viewer.require(&pull_url)?.clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let live = live_pull(&state, &repo, number).await?;
    if !(can_write(&repo) || is_author(&repo, &live.pull)) {
        return Err(WebError::Forbidden);
    }
    let reopen = match form.action.as_str() {
        "close" if live.pull.is_open() => false,
        "reopen"
            if !live.pull.is_open()
                && !live.pull.is_merged()
                && live.head_exists
                && live.base_exists =>
        {
            true
        }
        _ => return Ok(Redirect::to(&pull_url).into_response()),
    };
    if !form.body.trim().is_empty() {
        db::add_comment(
            &state.db,
            live.pull.issue_id,
            user.id,
            form.body.trim(),
            None,
        )
        .await?;
        events::comment(&state, &repo.repo, number, form.body.trim(), &user).await;
    }
    db::set_issue_state(&state.db, live.pull.issue_id, user.id, reopen).await?;
    let action = if reopen { "reopened" } else { "closed" };
    events::pull_request(&state, &repo.repo, number, action, &user).await;
    Ok(Redirect::to(&format!("{pull_url}#bottom")).into_response())
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

pub async fn delete_branch(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let pull_url = format!("{}/pull/{number}", repo.url);
    let user = repo.viewer.require(&pull_url)?.clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let live = live_pull(&state, &repo, number).await?;
    if !can_write(&repo) {
        return Err(WebError::Forbidden);
    }
    // Only a merged branch, and only if nothing was pushed to it since.
    if live.pull.is_merged()
        && repo
            .git
            .delete_branch(&live.pull.head_branch, &live.head_sha)
            .await?
    {
        db::add_event(
            &state.db,
            live.pull.issue_id,
            user.id,
            "branch_deleted",
            &live.pull.head_branch,
        )
        .await?;
        let update = RefUpdate {
            old: live.head_sha.clone(),
            new: "0".repeat(40),
            name: format!("refs/heads/{}", live.pull.head_branch),
        };
        if let Err(error) = events::push(&state, &repo.repo, &user, &update).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "couldn't record branch deletion"
            );
        }
    }
    Ok(Redirect::to(&format!("{pull_url}#bottom")).into_response())
}
