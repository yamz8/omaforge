//! Repository settings: description, visibility, collaborators, renaming and deleting.
//! Only the owner and site admins get here.

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect},
    Form,
};
use serde::Deserialize;

use super::{
    page,
    repo::{load, Loaded, RepoContext},
    Viewer, WebError, WebResult,
};
use crate::{db, AppState};

struct CollaboratorView {
    user_id: i64,
    username: String,
    role: String,
}

struct RuleView {
    id: i64,
    pattern: String,
    summary: String,
}

struct DeliveryView {
    id: i64,
    event: String,
    /// `succeeded`, `failed` or `pending`.
    status: String,
    detail: String,
    time_ago: String,
    payload: String,
    response: String,
}

struct WebhookView {
    id: i64,
    url: String,
    events: String,
    deliveries: Vec<DeliveryView>,
}

/// Events a webhook can subscribe to, with labels for the form.
const WEBHOOK_EVENTS: &[(&str, &str)] = &[
    ("push", "Pushes"),
    ("pull_request", "Pull requests"),
    ("pull_request_review", "Reviews"),
    ("issues", "Issues"),
    ("issue_comment", "Comments"),
    ("create", "Branch or tag created"),
    ("delete", "Branch or tag deleted"),
];

#[derive(Template)]
#[template(path = "repo_settings.html")]
struct SettingsPage {
    viewer: Viewer,
    repo: RepoContext,
    collaborators: Vec<CollaboratorView>,
    /// Usernames that can be added (not the owner, not already collaborators).
    candidates: Vec<String>,
    notice: Option<&'static str>,
    error: Option<String>,
    full_name: String,
    rules: Vec<RuleView>,
    webhooks: Vec<WebhookView>,
    webhook_events: &'static [(&'static str, &'static str)],
}

/// Load a repository for its settings: signed in, and owner or site admin. Others get
/// "not found", as with any page they can't see.
async fn load_admin(
    state: &AppState,
    viewer: Viewer,
    owner: &str,
    name: &str,
) -> WebResult<Loaded> {
    let here = format!("/{owner}/{name}/settings");
    viewer.require(&here)?;
    let repo = load(state, viewer, owner, name).await?;
    if !repo.is_admin() {
        return Err(WebError::NotFound);
    }
    Ok(repo)
}

async fn settings_page(
    state: &AppState,
    repo: &Loaded,
    notice: Option<&'static str>,
    error: Option<String>,
) -> WebResult {
    let collaborators: Vec<CollaboratorView> = db::collaborators(&state.db, repo.repo.id)
        .await?
        .into_iter()
        .map(|(user_id, username, role)| CollaboratorView {
            user_id,
            username,
            role,
        })
        .collect();
    let candidates = db::all_users(&state.db)
        .await?
        .into_iter()
        .filter(|(id, _)| {
            *id != repo.repo.owner_id && !collaborators.iter().any(|c| c.user_id == *id)
        })
        .map(|(_, name)| name)
        .collect();
    let rules = db::branch_rules(&state.db, repo.repo.id)
        .await?
        .into_iter()
        .map(|r| {
            let mut parts = Vec::new();
            if r.require_pull_request {
                parts.push("changes through pull requests".to_owned());
            }
            if r.required_approvals > 0 {
                let n = r.required_approvals;
                parts.push(format!(
                    "{n} approving review{}",
                    if n == 1 { "" } else { "s" }
                ));
            }
            if r.block_force_push {
                parts.push("no force pushes".to_owned());
            }
            if r.block_deletion {
                parts.push("can't be deleted".to_owned());
            }
            RuleView {
                id: r.id,
                pattern: r.pattern,
                summary: parts.join(" · "),
            }
        })
        .collect();
    let mut webhooks = Vec::new();
    for hook in db::webhooks(&state.db, repo.repo.id).await? {
        let deliveries = db::recent_deliveries(&state.db, hook.id, 10)
            .await?
            .into_iter()
            .map(|d| DeliveryView {
                id: d.id,
                detail: match (d.response_status, &d.error) {
                    (Some(code), _) => format!("HTTP {code} in {} ms", d.duration_ms.unwrap_or(0)),
                    (None, Some(error)) => error.chars().take(120).collect(),
                    (None, None) => "Waiting to be sent".to_owned(),
                },
                time_ago: super::render::time_ago(d.created),
                // Pretty-printed, so it's readable in the page.
                payload: serde_json::from_str::<serde_json::Value>(&d.payload)
                    .and_then(|v| serde_json::to_string_pretty(&v))
                    .unwrap_or(d.payload),
                response: d.response_body.unwrap_or_default(),
                event: d.event,
                status: d.status,
            })
            .collect();
        webhooks.push(WebhookView {
            id: hook.id,
            url: hook.url,
            events: hook.events.replace(',', ", "),
            deliveries,
        });
    }
    page(SettingsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, "settings"),
        collaborators,
        candidates,
        notice,
        error,
        full_name: format!("{}/{}", repo.repo.owner_name, repo.repo.name),
        rules,
        webhooks,
        webhook_events: WEBHOOK_EVENTS,
    })
}

pub async fn show(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    settings_page(&state, &repo, None, None).await
}

#[derive(Deserialize)]
pub struct GeneralForm {
    csrf: String,
    #[serde(default)]
    description: String,
    /// `public` or `private`.
    visibility: String,
}

pub async fn general(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<GeneralForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    let description: String = form.description.trim().chars().take(350).collect();
    db::set_repo_description(&state.db, repo.repo.owner_id, &repo.repo.name, &description).await?;
    db::set_repo_visibility(&state.db, repo.repo.id, form.visibility != "public").await?;
    // Reload so the page shows the new values.
    let repo = load_admin(&state, repo.viewer.clone(), &owner, &name).await?;
    settings_page(&state, &repo, Some("Settings saved."), None).await
}

#[derive(Deserialize)]
pub struct CollaboratorForm {
    csrf: String,
    username: String,
    /// `read` or `write`.
    role: String,
}

pub async fn add_collaborator(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<CollaboratorForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    let error = match db::user_by_username(&state.db, form.username.trim()).await? {
        _ if !matches!(form.role.as_str(), "read" | "write") => {
            Some("Choose read or write access.".to_owned())
        }
        None => Some(format!("There's no user named {:?}.", form.username.trim())),
        Some(user) if user.id == repo.repo.owner_id => {
            Some("The owner already has full access.".to_owned())
        }
        Some(user) => {
            db::set_collaborator(&state.db, repo.repo.id, user.id, &form.role).await?;
            tracing::info!(repo = %repo.url, collaborator = %user.username, role = %form.role, "collaborator set");
            None
        }
    };
    match error {
        Some(error) => settings_page(&state, &repo, None, Some(error)).await,
        None => Ok(Redirect::to(&format!("{}/settings#collaborators", repo.url)).into_response()),
    }
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

pub async fn remove_collaborator(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, user_id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    db::remove_collaborator(&state.db, repo.repo.id, user_id).await?;
    Ok(Redirect::to(&format!("{}/settings#collaborators", repo.url)).into_response())
}

#[derive(Deserialize)]
pub struct RenameForm {
    csrf: String,
    name: String,
}

pub async fn rename(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<RenameForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    let new_name = form.name.trim();
    if !forge_git::is_valid_name(new_name) {
        let error =
            "Use letters, numbers, '-', '_' and '.', not starting with '.' or ending in '.git'.";
        return settings_page(&state, &repo, None, Some(error.into())).await;
    }
    if new_name == repo.repo.name {
        return Ok(Redirect::to(&format!("{}/settings", repo.url)).into_response());
    }
    if let Err(error) = db::rename_repo(&state.db, &state.repos, &repo.repo, new_name).await {
        return settings_page(&state, &repo, None, Some(error.to_string())).await;
    }
    tracing::info!(from = %repo.url, to = new_name, "renamed repository");
    Ok(Redirect::to(&format!("/{}/{new_name}/settings", repo.repo.owner_name)).into_response())
}

#[derive(Deserialize)]
pub struct DeleteForm {
    csrf: String,
    /// Must be `owner/name`, typed out.
    confirm: String,
}

pub async fn delete(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<DeleteForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    let full_name = format!("{}/{}", repo.repo.owner_name, repo.repo.name);
    if form.confirm.trim() != full_name {
        let error = format!("Type {full_name} to confirm. Nothing was deleted.");
        return settings_page(&state, &repo, None, Some(error)).await;
    }
    db::destroy_repo(&state.db, &state.repos, &repo.repo).await?;
    tracing::info!(repo = %full_name, by = ?repo.viewer.user.as_ref().map(|u| &u.username), "deleted repository");
    Ok(Redirect::to(&format!("/{}", repo.repo.owner_name)).into_response())
}

// ---- Branch protection ----

#[derive(Deserialize)]
pub struct RuleForm {
    csrf: String,
    pattern: String,
    require_pull_request: Option<String>,
    #[serde(default)]
    required_approvals: i64,
    block_force_push: Option<String>,
    block_deletion: Option<String>,
}

pub async fn add_rule(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<RuleForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    let pattern = form.pattern.trim();
    let error = if !db::is_valid_pattern(pattern) {
        Some(
            "Name a branch, or a pattern like release/* (letters, numbers, -, _, ., / and *)."
                .to_owned(),
        )
    } else {
        db::add_branch_rule(
            &state.db,
            repo.repo.id,
            &db::NewBranchRule {
                pattern,
                require_pull_request: form.require_pull_request.is_some(),
                required_approvals: form.required_approvals,
                block_force_push: form.block_force_push.is_some(),
                block_deletion: form.block_deletion.is_some(),
            },
        )
        .await
        .err()
        .map(|e| e.to_string())
    };
    match error {
        Some(error) => settings_page(&state, &repo, None, Some(error)).await,
        None => Ok(Redirect::to(&format!("{}/settings#protection", repo.url)).into_response()),
    }
}

pub async fn delete_rule(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    db::delete_branch_rule(&state.db, repo.repo.id, id).await?;
    Ok(Redirect::to(&format!("{}/settings#protection", repo.url)).into_response())
}

// ---- Webhooks ----

#[derive(Deserialize)]
pub struct WebhookForm {
    csrf: String,
    url: String,
    #[serde(default)]
    secret: String,
    /// Checked events; repeated fields.
    #[serde(default)]
    event: Vec<String>,
}

pub async fn add_webhook(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
    axum_extra::extract::Form(form): axum_extra::extract::Form<WebhookForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    let user = repo
        .viewer
        .require(&format!("{}/settings", repo.url))?
        .clone();
    repo.viewer.check_csrf(&form.csrf)?;
    let url = form.url.trim();
    let events: Vec<&str> = form
        .event
        .iter()
        .map(String::as_str)
        .filter(|e| WEBHOOK_EVENTS.iter().any(|(name, _)| name == e))
        .collect();
    let error = if !(url.starts_with("https://") || url.starts_with("http://"))
        || url.len() > 2000
        || url.contains(char::is_whitespace)
    {
        Some("Enter an http:// or https:// URL.")
    } else if events.is_empty() {
        Some("Choose at least one event.")
    } else {
        None
    };
    if let Some(error) = error {
        return settings_page(&state, &repo, None, Some(error.into())).await;
    }
    let id = db::add_webhook(
        &state.db,
        repo.repo.id,
        url,
        form.secret.trim(),
        &events.join(","),
    )
    .await?;
    crate::events::ping(&state, &repo.repo, id, &user).await?;
    Ok(Redirect::to(&format!("{}/settings#webhooks", repo.url)).into_response())
}

pub async fn delete_webhook(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    db::delete_webhook(&state.db, repo.repo.id, id).await?;
    Ok(Redirect::to(&format!("{}/settings#webhooks", repo.url)).into_response())
}

pub async fn redeliver(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let repo = load_admin(&state, viewer, &owner, &name).await?;
    repo.viewer.check_csrf(&form.csrf)?;
    db::redeliver(&state.db, repo.repo.id, id, &crate::events::guid()).await?;
    Ok(Redirect::to(&format!("{}/settings#webhooks", repo.url)).into_response())
}
