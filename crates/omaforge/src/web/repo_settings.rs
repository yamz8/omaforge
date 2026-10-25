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
    page(SettingsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, "settings"),
        collaborators,
        candidates,
        notice,
        error,
        full_name: format!("{}/{}", repo.repo.owner_name, repo.repo.name),
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
