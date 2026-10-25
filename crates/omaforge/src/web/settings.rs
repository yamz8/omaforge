//! Account settings (SSH keys, access tokens, password) and creating repositories.

use askama::Template;
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect},
    Form,
};
use serde::Deserialize;

use super::{page, session::Viewer, WebResult};
use crate::{auth, db, git_ssh, AppState};

struct CredentialView {
    id: i64,
    name: String,
    fingerprint: String,
    added: String,
    last_used: String,
}

fn credential_views(credentials: Vec<db::Credential>) -> Vec<CredentialView> {
    credentials
        .into_iter()
        .map(|c| CredentialView {
            id: c.id,
            name: c.name,
            fingerprint: c.fingerprint,
            added: c.created_at.get(..10).unwrap_or(&c.created_at).to_owned(),
            last_used: c.last_used_at.map_or_else(
                || "Never used".to_owned(),
                |t| format!("Last used {}", t.get(..10).unwrap_or(&t)),
            ),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    viewer: Viewer,
    /// `keys`, `tokens` or `password`.
    section: &'static str,
    credentials: Vec<CredentialView>,
    /// A token just created, shown once.
    new_token: Option<String>,
    error: Option<String>,
    notice: Option<&'static str>,
    /// Values to put back in the form after an error.
    name: String,
    key: String,
    runners: Vec<RunnerView>,
    server_url: String,
}

struct RunnerView {
    id: i64,
    name: String,
    labels: String,
    version: String,
    seen: String,
    online: bool,
}

impl SettingsPage {
    fn new(viewer: Viewer, section: &'static str) -> Self {
        Self {
            viewer,
            section,
            credentials: Vec::new(),
            new_token: None,
            error: None,
            notice: None,
            name: String::new(),
            key: String::new(),
            runners: Vec::new(),
            server_url: String::new(),
        }
    }
}

pub async fn index() -> Redirect {
    Redirect::to("/settings/keys")
}

// ---- SSH keys ----

pub async fn keys(State(state): State<AppState>, viewer: Viewer) -> WebResult {
    let user = viewer.require("/settings/keys")?;
    let credentials = credential_views(db::ssh_keys(&state.db, user.id).await?);
    page(SettingsPage {
        credentials,
        ..SettingsPage::new(viewer, "keys")
    })
}

#[derive(Deserialize)]
pub struct AddKeyForm {
    csrf: String,
    name: String,
    key: String,
}

pub async fn add_key(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<AddKeyForm>,
) -> WebResult {
    let user = viewer.require("/settings/keys")?.clone();
    viewer.check_csrf(&form.csrf)?;
    let name = form.name.trim();
    let result = match git_ssh::parse_public_key(&form.key) {
        Ok(_) if name.is_empty() => Err("Give the key a name, like the computer it's on.".to_owned()),
        Ok((public_key, fingerprint)) => db::add_ssh_key(&state.db, user.id, name, &public_key, &fingerprint)
            .await
            .map_err(|e| e.to_string()),
        Err(_) => Err("That doesn't look like an OpenSSH public key (it should start with ssh-ed25519, ssh-rsa or ecdsa-…).".to_owned()),
    };
    match result {
        Ok(()) => Ok(Redirect::to("/settings/keys").into_response()),
        Err(error) => {
            let credentials = credential_views(db::ssh_keys(&state.db, user.id).await?);
            page(SettingsPage {
                credentials,
                error: Some(error),
                name: name.to_owned(),
                key: form.key,
                ..SettingsPage::new(viewer, "keys")
            })
        }
    }
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

pub async fn delete_key(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let user = viewer.require("/settings/keys")?;
    viewer.check_csrf(&form.csrf)?;
    db::delete_ssh_key(&state.db, user.id, id).await?;
    Ok(Redirect::to("/settings/keys").into_response())
}

// ---- Access tokens ----

pub async fn tokens(State(state): State<AppState>, viewer: Viewer) -> WebResult {
    let user = viewer.require("/settings/tokens")?;
    let credentials = credential_views(db::tokens(&state.db, user.id).await?);
    page(SettingsPage {
        credentials,
        ..SettingsPage::new(viewer, "tokens")
    })
}

#[derive(Deserialize)]
pub struct CreateTokenForm {
    csrf: String,
    name: String,
}

pub async fn create_token(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<CreateTokenForm>,
) -> WebResult {
    let user = viewer.require("/settings/tokens")?.clone();
    viewer.check_csrf(&form.csrf)?;
    let name = form.name.trim();
    let mut view = SettingsPage::new(viewer, "tokens");
    if name.is_empty() {
        view.error = Some("Give the token a name, like what it's for.".to_owned());
    } else {
        let (token, hash) = auth::new_token()?;
        db::create_token(&state.db, user.id, name, &hash).await?;
        view.new_token = Some(token);
    }
    view.credentials = credential_views(db::tokens(&state.db, user.id).await?);
    page(view)
}

pub async fn delete_token(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    let user = viewer.require("/settings/tokens")?;
    viewer.check_csrf(&form.csrf)?;
    db::delete_token(&state.db, user.id, id).await?;
    Ok(Redirect::to("/settings/tokens").into_response())
}

// ---- Runners (site admins) ----

/// A site admin, or a redirect to sign in / a 404 for everyone else.
fn require_admin<'a>(viewer: &'a Viewer, here: &str) -> WebResult<&'a db::User> {
    let user = viewer.require(here)?;
    if user.is_admin {
        Ok(user)
    } else {
        Err(super::WebError::NotFound)
    }
}

async fn runners_page(state: &AppState, viewer: Viewer) -> WebResult<SettingsPage> {
    let now = super::render::now();
    let runners = db::runners(&state.db)
        .await?
        .into_iter()
        .map(|r| RunnerView {
            id: r.id,
            labels: if r.labels.is_empty() {
                "no labels yet".into()
            } else {
                r.labels.replace(',', ", ")
            },
            version: r.version,
            seen: r.last_seen.map_or_else(
                || "Never connected".to_owned(),
                |t| format!("Last seen {}", super::render::time_ago(t)),
            ),
            // A waiting runner checks in at least every 30 seconds.
            online: r.last_seen.is_some_and(|t| now - t < 90),
            name: r.name,
        })
        .collect();
    Ok(SettingsPage {
        runners,
        server_url: state.site.http_url.clone(),
        ..SettingsPage::new(viewer, "runners")
    })
}

pub async fn runners(State(state): State<AppState>, viewer: Viewer) -> WebResult {
    require_admin(&viewer, "/settings/runners")?;
    page(runners_page(&state, viewer).await?)
}

pub async fn create_runner(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<CreateTokenForm>,
) -> WebResult {
    require_admin(&viewer, "/settings/runners")?;
    viewer.check_csrf(&form.csrf)?;
    let name = form.name.trim().to_owned();
    let (token, hash) = crate::ci::new_runner_token()?;
    let result = if name.is_empty() || name.len() > 100 {
        Err(
            "Give the runner a name of up to 100 characters, like the machine it runs on."
                .to_owned(),
        )
    } else {
        db::create_runner(&state.db, &name, &hash)
            .await
            .map_err(|e| format!("{e:#}"))
    };
    let mut view = runners_page(&state, viewer).await?;
    match result {
        Ok(_) => view.new_token = Some(token),
        Err(error) => {
            view.error = Some(error);
            view.name = name;
        }
    }
    page(view)
}

pub async fn delete_runner(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    require_admin(&viewer, "/settings/runners")?;
    viewer.check_csrf(&form.csrf)?;
    db::delete_runner(&state.db, id).await?;
    Ok(Redirect::to("/settings/runners").into_response())
}

// ---- Password ----

pub async fn password(viewer: Viewer) -> WebResult {
    viewer.require("/settings/password")?;
    page(SettingsPage::new(viewer, "password"))
}

#[derive(Deserialize)]
pub struct PasswordForm {
    csrf: String,
    current: String,
    new: String,
    confirm: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<PasswordForm>,
) -> WebResult {
    let user = viewer.require("/settings/password")?.clone();
    viewer.check_csrf(&form.csrf)?;
    let current_ok = {
        let user = user.clone();
        let current = form.current;
        tokio::task::spawn_blocking(move || auth::verify_user_password(&current, Some(&user)))
            .await
            .map_err(anyhow::Error::from)?
    };
    let error = if !current_ok {
        Some("Your current password is incorrect.")
    } else if form.new.len() < 8 {
        Some("The new password must be at least 8 characters.")
    } else if form.new != form.confirm {
        Some("The new passwords don't match.")
    } else {
        None
    };
    let mut view = SettingsPage::new(viewer, "password");
    match error {
        Some(error) => view.error = Some(error.to_owned()),
        None => {
            let hash = tokio::task::spawn_blocking(move || auth::hash_password(&form.new))
                .await
                .map_err(anyhow::Error::from)??;
            db::set_password(&state.db, user.id, &hash).await?;
            // Changing the password signs out every other browser.
            db::delete_user_sessions(&state.db, user.id, view.viewer.session_hash()).await?;
            view.notice = Some("Password changed. Other browsers have been signed out.");
        }
    }
    page(view)
}

// ---- New repository ----

#[derive(Template)]
#[template(path = "new_repo.html")]
struct NewRepoPage {
    viewer: Viewer,
    owner: String,
    name: String,
    description: String,
    private: bool,
    error: Option<String>,
}

pub async fn new_repo_form(viewer: Viewer) -> WebResult {
    let owner = viewer.require("/new")?.username.clone();
    page(NewRepoPage {
        viewer,
        owner,
        name: String::new(),
        description: String::new(),
        private: true,
        error: None,
    })
}

#[derive(Deserialize)]
pub struct NewRepoForm {
    csrf: String,
    name: String,
    description: String,
    /// "public" or "private".
    visibility: String,
}

pub async fn create_repo(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<NewRepoForm>,
) -> WebResult {
    let user = viewer.require("/new")?.clone();
    viewer.check_csrf(&form.csrf)?;
    let name = form.name.trim();
    let description = form.description.trim();
    let private = form.visibility != "public";
    let result = if !forge_git::is_valid_name(name) {
        Err(
            "Use letters, numbers, '-', '_' and '.', not starting with '.' or ending in '.git'."
                .to_owned(),
        )
    } else {
        db::create_repo(&state.db, &state.repos, &user, name, description, private)
            .await
            .map_err(|e| e.to_string())
    };
    match result {
        Ok(_) => {
            tracing::info!(user = %user.username, repo = name, "created repository");
            Ok(Redirect::to(&format!("/{}/{name}", user.username)).into_response())
        }
        Err(error) => page(NewRepoPage {
            viewer,
            owner: user.username,
            name: name.to_owned(),
            description: description.to_owned(),
            private,
            error: Some(error),
        }),
    }
}
