//! A GitHub-compatible API, enough for the `gh` CLI (and scripts) to work against
//! omaforge: REST under `/api/v3` and GraphQL at `/api/graphql`, the paths GitHub
//! Enterprise uses, so `GH_HOST=forge.example.com gh pr list` just works.

mod graphql;

use std::sync::OnceLock;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::{
    auth::{self, Access, Identity},
    db, AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/graphql", post(graphql_handler))
        .route("/api/v3", get(root))
        .route("/api/v3/", get(root))
        .route("/api/v3/meta", get(meta))
        .route("/api/v3/user", get(current_user))
        .route("/api/v3/users/{username}", get(user))
        .route("/api/v3/repos/{owner}/{repo}", get(repository))
        .route(
            "/api/v3/repos/{owner}/{repo}/pulls/{number}",
            get(pull_request),
        )
        .route(
            "/api/v3/{*rest}",
            get(not_found)
                .post(not_found)
                .patch(not_found)
                .put(not_found)
                .delete(not_found),
        )
}

fn schema() -> &'static graphql::Schema {
    static SCHEMA: OnceLock<graphql::Schema> = OnceLock::new();
    SCHEMA.get_or_init(graphql::schema)
}

/// A GitHub-style error body.
fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({ "message": message, "documentation_url": "https://docs.github.com/rest" })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
async fn viewer(state: &AppState, headers: &HeaderMap) -> Result<Option<db::User>, Response> {
    match auth::authenticate(&state.db, headers).await {
        Ok(Identity::User(user)) => Ok(Some(user)),
        Ok(Identity::Anonymous) => Ok(None),
        Ok(Identity::Invalid) => Err(error(StatusCode::UNAUTHORIZED, "Bad credentials")),
        Err(e) => {
            tracing::error!(error = %e, "API authentication failed");
            Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Server error"))
        }
    }
}

async fn graphql_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<async_graphql::Request>,
) -> Response {
    let viewer = match viewer(&state, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let request = request.data(graphql::ApiContext { state, viewer });
    Json(schema().execute(request).await).into_response()
}

async fn root(State(state): State<AppState>) -> Json<Value> {
    let base = format!("{}/api/v3", state.site.http_url);
    Json(json!({
        "current_user_url": format!("{base}/user"),
        "repository_url": format!("{base}/repos/{{owner}}/{{repo}}"),
        "user_url": format!("{base}/users/{{user}}"),
    }))
}

/// GitHub Enterprise reports its version here; clients use it to pick features.
async fn meta() -> Json<Value> {
    Json(json!({ "installed_version": "3.17.0", "verifiable_password_authentication": true }))
}

pub(crate) fn user_json(state: &AppState, id: i64, login: &str) -> Value {
    json!({
        "login": login,
        "id": id,
        "node_id": format!("U_{id}"),
        "type": "User",
        "site_admin": false,
        "name": login,
        "html_url": format!("{}/{login}", state.site.http_url),
        "url": format!("{}/api/v3/users/{login}", state.site.http_url),
        "avatar_url": "",
    })
}

async fn current_user(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = match viewer(&state, &headers).await {
        Ok(Some(user)) => user,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "Requires authentication"),
        Err(response) => return response,
    };
    let mut body = user_json(&state, user.id, &user.username);
    body["site_admin"] = json!(user.is_admin);
    let mut response = Json(body).into_response();
    // `gh auth` checks a token's scopes; omaforge tokens can do everything their user can.
    response.headers_mut().insert(
        "x-oauth-scopes",
        HeaderValue::from_static("repo, read:org, gist, workflow"),
    );
    response
}

async fn user(State(state): State<AppState>, Path(username): Path<String>) -> Response {
    match db::user_by_username(&state.db, &username).await {
        Ok(Some(user)) => Json(user_json(&state, user.id, &user.username)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "Not Found"),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Server error"),
    }
}

async fn repository(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let viewer = match viewer(&state, &headers).await {
        Ok(viewer) => viewer,
        Err(response) => return response,
    };
    let repo = match db::repo_by_path(&state.db, &owner, &name).await {
        Ok(Some(repo)) => repo,
        Ok(None) => return error(StatusCode::NOT_FOUND, "Not Found"),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Server error"),
    };
    let access = auth::access(&state.db, viewer.as_ref(), &repo)
        .await
        .unwrap_or(Access::None);
    if access < Access::Read {
        return error(StatusCode::NOT_FOUND, "Not Found");
    }
    let git = forge_git::Repository::open(forge_git::repo_path(
        &state.repos,
        &repo.owner_name,
        &repo.name,
    ));
    let default_branch = git
        .default_branch()
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "main".into());
    Json(repo_json(&state, &repo, access, &default_branch)).into_response()
}

pub(crate) fn repo_json(
    state: &AppState,
    repo: &db::Repo,
    access: Access,
    default_branch: &str,
) -> Value {
    let full_name = format!("{}/{}", repo.owner_name, repo.name);
    json!({
        "id": repo.id,
        "node_id": format!("R_{}", repo.id),
        "name": repo.name,
        "full_name": full_name,
        "private": repo.is_private,
        "visibility": if repo.is_private { "private" } else { "public" },
        "owner": user_json(state, repo.owner_id, &repo.owner_name),
        "description": repo.description,
        "html_url": format!("{}/{full_name}", state.site.http_url),
        "clone_url": state.site.http_clone_url(&repo.owner_name, &repo.name),
        "ssh_url": state.site.ssh_clone_url(&repo.owner_name, &repo.name),
        "default_branch": default_branch,
        "fork": false,
        "archived": false,
        "has_issues": true,
        "has_wiki": false,
        "permissions": {
            "admin": access >= Access::Admin,
            "maintain": access >= Access::Admin,
            "push": access >= Access::Write,
            "triage": access >= Access::Write,
            "pull": access >= Access::Read,
        },
    })
}

/// A repository the caller can read, with their access.
#[allow(clippy::result_large_err)]
async fn readable_repo(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<(db::Repo, Access), Response> {
    let viewer = viewer(state, headers).await?;
    let repo = match db::repo_by_path(&state.db, owner, name).await {
        Ok(Some(repo)) => repo,
        Ok(None) => return Err(error(StatusCode::NOT_FOUND, "Not Found")),
        Err(_) => return Err(error(StatusCode::INTERNAL_SERVER_ERROR, "Server error")),
    };
    let access = auth::access(&state.db, viewer.as_ref(), &repo)
        .await
        .unwrap_or(Access::None);
    if access < Access::Read {
        return Err(error(StatusCode::NOT_FOUND, "Not Found"));
    }
    Ok((repo, access))
}

/// A pull request as JSON, or as a unified diff with `Accept: application/vnd.github.diff`
/// (what `gh pr diff` asks for).
async fn pull_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> Response {
    let (repo, _) = match readable_repo(&state, &headers, &owner, &name).await {
        Ok(found) => found,
        Err(response) => return response,
    };
    let Ok(Some(pull)) = db::pull(&state.db, repo.id, number).await else {
        return error(StatusCode::NOT_FOUND, "Not Found");
    };
    let git = forge_git::Repository::open(forge_git::repo_path(
        &state.repos,
        &repo.owner_name,
        &repo.name,
    ));
    let Ok(tips) = crate::actions::pull_tips(&state, &git, &pull).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Server error");
    };
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if accept.contains("diff") || accept.contains("patch") {
        return match git.compare_patch(&tips.base_sha, &tips.head_sha).await {
            Ok(patch) => (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                patch,
            )
                .into_response(),
            Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "Server error"),
        };
    }
    let full_name = format!("{}/{}", repo.owner_name, repo.name);
    let state_name = if pull.is_open() { "open" } else { "closed" };
    Json(json!({
        "number": pull.number,
        "node_id": format!("PR_{}", pull.issue_id),
        "title": pull.title,
        "body": pull.body,
        "state": state_name,
        "merged": pull.is_merged(),
        "merge_commit_sha": pull.merge_sha,
        "html_url": format!("{}/{full_name}/pull/{}", state.site.http_url, pull.number),
        "user": user_json(&state, pull.author_id, &pull.author),
        "base": { "ref": pull.base_branch, "sha": tips.base_sha },
        "head": { "ref": pull.head_branch, "sha": tips.head_sha },
        "created_at": crate::web::render::iso8601(pull.created),
    }))
    .into_response()
}

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Not Found")
}
