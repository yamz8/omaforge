//! Issues: listing, opening, discussing, labels and assignees.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Redirect},
};
use axum_extra::extract::Form;
use serde::Deserialize;

use super::{
    conversation::{self, Mentions, Timeline, TimelineEntry},
    page, render,
    repo::{load, Loaded, RepoContext},
    Viewer, WebError, WebResult,
};
use crate::{db, AppState};

fn can_write(repo: &Loaded) -> bool {
    repo.can_write()
}

/// A label as shown on a chip: its colors are computed so the text stays readable.
pub(super) struct LabelView {
    id: i64,
    name: String,
    description: String,
    /// `background: #rrggbb; color: #...`; built only from validated hex colors.
    style: String,
    url: String,
    open_count: i64,
}

fn label_view(repo: &Loaded, label: &db::Label) -> LabelView {
    LabelView {
        id: label.id,
        name: label.name.clone(),
        description: label.description.clone(),
        style: label_style(&label.color),
        url: format!(
            "{}/issues?label={}",
            repo.url,
            super::form_urlencode(&label.name)
        ),
        open_count: label.open_count,
    }
}

/// Inline style for a label chip, with black or white text depending on the color.
fn label_style(color: &str) -> String {
    let hex = color
        .strip_prefix('#')
        .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    let Some(hex) = hex else {
        return String::new();
    };
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
    let luminance = 0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4);
    let text = if luminance > 0.55 {
        "#1f2328"
    } else {
        "#ffffff"
    };
    format!("background: #{hex}; color: {text};")
}

// ---- List ----

struct IssueListItem {
    number: i64,
    title: String,
    url: String,
    open: bool,
    author: String,
    created_ago: String,
    comment_count: i64,
    labels: Vec<LabelView>,
    assignees: Vec<String>,
}

#[derive(Template)]
#[template(path = "issues.html")]
struct IssuesPage {
    viewer: Viewer,
    repo: RepoContext,
    showing_open: bool,
    open_count: i64,
    closed_count: i64,
    issues: Vec<IssueListItem>,
    labels: Vec<LabelView>,
    label: String,
    search: String,
    /// The list URL keeping the label and search, for the open/closed tabs.
    filter_query: String,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
    label: Option<String>,
    q: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let showing_open = query.state.as_deref() != Some("closed");
    let label = query.label.filter(|l| !l.is_empty());
    let search = query
        .q
        .map(|q| q.trim().to_owned())
        .filter(|q| !q.is_empty());
    let (open_count, closed_count) = db::issue_counts(&state.db, repo.repo.id).await?;
    let mut labels_by_issue = db::labels_by_issue(&state.db, repo.repo.id).await?;
    let mut assignees = db::assignees_by_issue(&state.db, repo.repo.id).await?;
    let state_name = if showing_open { "open" } else { "closed" };
    let issues = db::issues(
        &state.db,
        repo.repo.id,
        state_name,
        label.as_deref(),
        search.as_deref(),
    )
    .await?
    .into_iter()
    .map(|i| IssueListItem {
        url: format!("{}/issues/{}", repo.url, i.number),
        number: i.number,
        open: i.is_open(),
        author: i.author,
        created_ago: render::time_ago(i.created),
        comment_count: i.comment_count,
        labels: labels_by_issue
            .remove(&i.issue_id)
            .unwrap_or_default()
            .iter()
            .map(|l| label_view(&repo, l))
            .collect(),
        assignees: assignees.remove(&i.issue_id).unwrap_or_default(),
        title: i.title,
    })
    .collect();
    let mut filter_query = String::new();
    if let Some(label) = &label {
        filter_query.push_str(&format!("&label={}", super::form_urlencode(label)));
    }
    if let Some(search) = &search {
        filter_query.push_str(&format!("&q={}", super::form_urlencode(search)));
    }
    let labels = db::labels(&state.db, repo.repo.id)
        .await?
        .iter()
        .map(|l| label_view(&repo, l))
        .collect();
    page(IssuesPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "issues"),
        showing_open,
        open_count,
        closed_count,
        issues,
        labels,
        label: label.unwrap_or_default(),
        search: search.unwrap_or_default(),
        filter_query,
    })
}

// ---- New issue ----

#[derive(Template)]
#[template(path = "new_issue.html")]
struct NewIssuePage {
    viewer: Viewer,
    repo: RepoContext,
    title: String,
    body: String,
    error: Option<&'static str>,
}

pub async fn new_form(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    repo.viewer.require(&format!("{}/issues/new", repo.url))?;
    page(NewIssuePage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "issues"),
        title: String::new(),
        body: String::new(),
        error: None,
    })
}

#[derive(Deserialize)]
pub struct NewIssueForm {
    csrf: String,
    title: String,
    #[serde(default)]
    body: String,
}

pub async fn create(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<NewIssueForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let user = repo
        .viewer
        .require(&format!("{}/issues/new", repo.url))?
        .clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let title = form.title.trim();
    if title.is_empty() {
        return page(NewIssuePage {
            viewer: repo.viewer.clone(),
            repo: repo.context(&state, "issues"),
            title: String::new(),
            body: form.body,
            error: Some("Give the issue a title."),
        });
    }
    let number =
        db::create_issue(&state.db, repo.repo.id, user.id, title, form.body.trim()).await?;
    tracing::info!(repo = %repo.url, number, user = %user.username, "opened issue");
    Ok(Redirect::to(&format!("{}/issues/{number}", repo.url)).into_response())
}

// ---- Issue page ----

struct Choice {
    id: i64,
    name: String,
    chosen: bool,
    /// For labels: the chip style.
    style: String,
}

#[derive(Template)]
#[template(path = "issue.html")]
struct IssuePage {
    viewer: Viewer,
    repo: RepoContext,
    number: i64,
    title: String,
    open: bool,
    author: String,
    created_ago: String,
    description: String,
    comment_count: i64,
    timeline: Vec<TimelineEntry>,
    labels: Vec<LabelView>,
    assignees: Vec<String>,
    /// For people who can change labels and assignees.
    label_choices: Vec<Choice>,
    assignee_choices: Vec<Choice>,
    can_triage: bool,
    can_close: bool,
    url: String,
}

/// An issue, or a redirect if the number belongs to a pull request.
async fn load_issue(
    state: &AppState,
    repo: &Loaded,
    number: i64,
) -> WebResult<Result<db::Issue, Redirect>> {
    match db::issue(&state.db, repo.repo.id, number).await? {
        Some(issue) => Ok(Ok(issue)),
        None if db::number_kind(&state.db, repo.repo.id, number).await? == Some(true) => Ok(Err(
            Redirect::permanent(&format!("{}/pull/{number}", repo.url)),
        )),
        None => Err(WebError::NotFound),
    }
}

pub async fn show(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let issue = match load_issue(&state, &repo, number).await? {
        Ok(issue) => issue,
        Err(redirect) => return Ok(redirect.into_response()),
    };
    let mentions = Mentions::load(&state, &repo).await?;
    let mut timeline = Timeline::default();
    for c in db::comments(&state.db, issue.issue_id).await? {
        timeline.push(c.created, conversation::comment(c, &mentions, None));
    }
    for e in db::events(&state.db, issue.issue_id).await? {
        timeline.push(e.created, conversation::event(e, &repo.url, None));
    }
    let labels: Vec<db::Label> = db::labels_by_issue(&state.db, repo.repo.id)
        .await?
        .remove(&issue.issue_id)
        .unwrap_or_default();
    let assignees = db::assignees(&state.db, issue.issue_id).await?;
    let can_triage = can_write(&repo);
    let (label_choices, assignee_choices) = if can_triage {
        let chosen: Vec<i64> = labels.iter().map(|l| l.id).collect();
        let label_choices = db::labels(&state.db, repo.repo.id)
            .await?
            .into_iter()
            .map(|l| Choice {
                chosen: chosen.contains(&l.id),
                style: label_style(&l.color),
                id: l.id,
                name: l.name,
            })
            .collect();
        let assigned: Vec<i64> = assignees.iter().map(|(id, _)| *id).collect();
        let assignee_choices = db::all_users(&state.db)
            .await?
            .into_iter()
            .map(|(id, name)| Choice {
                chosen: assigned.contains(&id),
                style: String::new(),
                id,
                name,
            })
            .collect();
        (label_choices, assignee_choices)
    } else {
        (Vec::new(), Vec::new())
    };
    let is_author = repo
        .viewer
        .user
        .as_ref()
        .is_some_and(|u| u.id == issue.author_id);
    page(IssuePage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "issues"),
        url: format!("{}/issues/{}", repo.url, issue.number),
        number: issue.number,
        open: issue.is_open(),
        description: mentions.render(&issue.body),
        title: issue.title,
        author: issue.author,
        created_ago: render::time_ago(issue.created),
        comment_count: issue.comment_count,
        timeline: timeline.finish(),
        labels: labels.iter().map(|l| label_view(&repo, l)).collect(),
        assignees: assignees.into_iter().map(|(_, name)| name).collect(),
        label_choices,
        assignee_choices,
        can_triage,
        can_close: can_triage || is_author,
    })
}

// ---- Actions ----

#[derive(Deserialize)]
pub struct CommentForm {
    csrf: String,
    #[serde(default)]
    body: String,
    /// `close` or `reopen`, when the comment form's other buttons were used.
    action: Option<String>,
}

/// Comment, and optionally close or reopen at the same time.
pub async fn comment(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<CommentForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let url = format!("{}/issues/{number}", repo.url);
    let user = repo.viewer.require(&url)?.clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let issue = match load_issue(&state, &repo, number).await? {
        Ok(issue) => issue,
        Err(redirect) => return Ok(redirect.into_response()),
    };
    let body = form.body.trim();
    if !body.is_empty() {
        db::add_comment(&state.db, issue.issue_id, user.id, body, None).await?;
    }
    if let Some(action) = form.action.as_deref() {
        let allowed = can_write(&repo) || user.id == issue.author_id;
        if !allowed {
            return Err(WebError::Forbidden);
        }
        match action {
            "close" if issue.is_open() => {
                db::set_issue_state(&state.db, issue.issue_id, user.id, false).await?
            }
            "reopen" if !issue.is_open() => {
                db::set_issue_state(&state.db, issue.issue_id, user.id, true).await?
            }
            _ => {}
        }
    }
    Ok(Redirect::to(&format!("{url}#bottom")).into_response())
}

#[derive(Deserialize)]
pub struct ChoicesForm {
    csrf: String,
    /// Checked ids; repeated fields (`id=1&id=3`).
    #[serde(default)]
    id: Vec<i64>,
}

pub async fn set_labels(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<ChoicesForm>,
) -> WebResult {
    let (repo, issue, user) = triage(&state, viewer, &owner, &name, number, &form.csrf).await?;
    db::set_issue_labels(&state.db, repo.repo.id, issue.issue_id, user.id, &form.id).await?;
    Ok(Redirect::to(&format!("{}/issues/{number}", repo.url)).into_response())
}

pub async fn set_assignees(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(form): Form<ChoicesForm>,
) -> WebResult {
    let (repo, issue, user) = triage(&state, viewer, &owner, &name, number, &form.csrf).await?;
    db::set_assignees(&state.db, issue.issue_id, user.id, &form.id).await?;
    Ok(Redirect::to(&format!("{}/issues/{number}", repo.url)).into_response())
}

/// Load an issue for changing its labels or assignees, which needs write access.
async fn triage(
    state: &AppState,
    viewer: Viewer,
    owner: &str,
    name: &str,
    number: i64,
    csrf: &str,
) -> WebResult<(Loaded, db::Issue, db::User)> {
    let repo = load(state, viewer, owner, name).await?;
    let user = repo
        .viewer
        .require(&format!("{}/issues/{number}", repo.url))?
        .clone();
    repo.viewer.check_csrf(csrf)?;
    if !can_write(&repo) {
        return Err(WebError::Forbidden);
    }
    let issue = db::issue(&state.db, repo.repo.id, number)
        .await?
        .ok_or(WebError::NotFound)?;
    Ok((repo, issue, user))
}

// ---- Labels ----

#[derive(Template)]
#[template(path = "labels.html")]
struct LabelsPage {
    viewer: Viewer,
    repo: RepoContext,
    labels: Vec<LabelView>,
    can_edit: bool,
    error: Option<String>,
    name: String,
    color: String,
    description: String,
}

pub async fn labels(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    labels_page(
        &state,
        &repo,
        None,
        String::new(),
        "#7aa2f7".into(),
        String::new(),
    )
    .await
}

async fn labels_page(
    state: &AppState,
    repo: &Loaded,
    error: Option<String>,
    name: String,
    color: String,
    description: String,
) -> WebResult {
    let labels = db::labels(&state.db, repo.repo.id)
        .await?
        .iter()
        .map(|l| label_view(repo, l))
        .collect();
    page(LabelsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, "issues"),
        labels,
        can_edit: can_write(repo),
        error,
        name,
        color,
        description,
    })
}

#[derive(Deserialize)]
pub struct LabelForm {
    csrf: String,
    name: String,
    color: String,
    #[serde(default)]
    description: String,
}

pub async fn create_label(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<LabelForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    repo.viewer.require(&format!("{}/labels", repo.url))?;
    repo.viewer.check_csrf(&form.csrf)?;
    if !can_write(&repo) {
        return Err(WebError::Forbidden);
    }
    let error = if !db::is_valid_label(&form.name, &form.color) {
        Some("Give the label a name (up to 50 characters) and a color like #7aa2f7.".to_owned())
    } else {
        db::create_label(
            &state.db,
            repo.repo.id,
            &form.name,
            &form.color,
            &form.description,
        )
        .await
        .err()
        .map(|e| e.to_string())
    };
    match error {
        None => Ok(Redirect::to(&format!("{}/labels", repo.url)).into_response()),
        Some(error) => {
            labels_page(
                &state,
                &repo,
                Some(error),
                form.name,
                form.color,
                form.description,
            )
            .await
        }
    }
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

pub async fn delete_label(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    repo.viewer.require(&format!("{}/labels", repo.url))?;
    repo.viewer.check_csrf(&form.csrf)?;
    if !can_write(&repo) {
        return Err(WebError::Forbidden);
    }
    db::delete_label(&state.db, repo.repo.id, id).await?;
    Ok(Redirect::to(&format!("{}/labels", repo.url)).into_response())
}

#[cfg(test)]
mod tests {
    use super::label_style;

    #[test]
    fn label_text_contrast() {
        assert_eq!(
            label_style("#ffffff"),
            "background: #ffffff; color: #1f2328;"
        );
        assert_eq!(
            label_style("#0075ca"),
            "background: #0075ca; color: #ffffff;"
        );
        // Anything else never reaches the style attribute.
        assert_eq!(label_style("red; background: url(x)"), "");
    }
}
