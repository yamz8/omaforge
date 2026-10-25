//! Git smart HTTP transport (protocol v0/v1/v2), backed by `git upload-pack` and
//! `git receive-pack` in `--stateless-rpc` mode.

use std::process::Stdio;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    auth::{self, Identity},
    db,
    git_service::{self, Decision, Service},
    AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
        // git gzips large upload-pack requests.
        .layer(RequestDecompressionLayer::new())
}

#[derive(Deserialize)]
struct InfoRefsQuery {
    service: Option<String>,
}

async fn info_refs(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    Query(query): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(service) = query.service.as_deref().and_then(Service::from_name) else {
        return (
            StatusCode::FORBIDDEN,
            "dumb HTTP protocol is not supported\n",
        )
            .into_response();
    };
    let grant = match authorize(&state, &owner, &repo, &headers, service).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };

    let protocol = git_protocol(&headers);
    let mut command = git_service::command(
        &state,
        service,
        &grant,
        &["--stateless-rpc", "--advertise-refs"],
        protocol.as_deref(),
    );
    let output = match command.output().await {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::error!(stderr = %String::from_utf8_lossy(&output.stderr), "advertise-refs failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => return internal_error(error.into()),
    };

    let mut body = Vec::new();
    // Protocol v2 responses start directly with the capability advertisement.
    if !protocol.as_deref().is_some_and(|p| p.contains("version=2")) {
        let line = format!("# service={}\n", service.name());
        body.extend_from_slice(format!("{:04x}{line}0000", line.len() + 4).as_bytes());
    }
    body.extend_from_slice(&output.stdout);

    git_response(
        &format!("application/x-{}-advertisement", service.name()),
        Body::from(body),
    )
}

async fn upload_pack(
    state: State<AppState>,
    path: Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    rpc(state, path, headers, body, Service::UploadPack).await
}

async fn receive_pack(
    state: State<AppState>,
    path: Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    rpc(state, path, headers, body, Service::ReceivePack).await
}

/// Stream the request body into a stateless-rpc git process and its stdout back out.
async fn rpc(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
    service: Service,
) -> Response {
    let grant = match authorize(&state, &owner, &repo, &headers, service).await {
        Ok(grant) => grant,
        Err(response) => return response,
    };

    let protocol = git_protocol(&headers);
    let mut command = git_service::command(
        &state,
        service,
        &grant,
        &["--stateless-rpc"],
        protocol.as_deref(),
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return internal_error(error.into()),
    };

    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    tokio::spawn(async move {
        let feed = async move {
            let mut chunks = body.into_data_stream();
            while let Some(chunk) = chunks.next().await {
                stdin.write_all(&chunk?).await?;
            }
            anyhow::Ok(())
        };
        let mut errors = String::new();
        let (fed, _) = tokio::join!(feed, stderr.read_to_string(&mut errors));
        if let Err(error) = fed {
            tracing::warn!(%error, "streaming request into git failed");
        }
        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => {
                tracing::warn!(%status, stderr = %errors.trim(), "{} failed", service.name())
            }
            Err(error) => tracing::warn!(%error, "waiting for git failed"),
        }
    });

    git_response(
        &format!("application/x-{}-result", service.name()),
        Body::from_stream(ReaderStream::new(stdout)),
    )
}

/// Authenticate the request and authorize `service` on the repo, returning its path.
#[allow(clippy::result_large_err)]
async fn authorize(
    state: &AppState,
    owner: &str,
    repo: &str,
    headers: &HeaderMap,
    service: Service,
) -> Result<git_service::Grant, Response> {
    // A CI job may clone (only) its own repository with its job token.
    if let Some(grant) = job_grant(state, owner, repo, headers, service).await? {
        return Ok(grant);
    }
    let user = match auth::authenticate(&state.db, headers).await {
        Ok(Identity::User(user)) => Some(user),
        Ok(Identity::Anonymous) => None,
        Ok(Identity::Invalid) => return Err(unauthorized()),
        Err(error) => return Err(internal_error(error)),
    };
    match git_service::authorize(state, user.as_ref(), owner, repo, service).await {
        Ok(Decision::Allowed(grant)) => Ok(grant),
        Ok(Decision::NeedsLogin) => Err(unauthorized()),
        Ok(Decision::ReadOnly) => Err((
            StatusCode::FORBIDDEN,
            "you do not have write access to this repository\n",
        )
            .into_response()),
        Ok(Decision::NotFound) => Err(StatusCode::NOT_FOUND.into_response()),
        Err(error) => Err(internal_error(error)),
    }
}

/// A job token in Basic auth (as git sends it) or a Bearer header.
#[allow(clippy::result_large_err)]
async fn job_grant(
    state: &AppState,
    owner: &str,
    repo: &str,
    headers: &HeaderMap,
    service: Service,
) -> Result<Option<git_service::Grant>, Response> {
    let Some(token) = auth::job_token(headers) else {
        return Ok(None);
    };
    let not_allowed = || Err(unauthorized());
    let job = match db::job_by_token_hash(&state.db, &auth::hash_token(&token)).await {
        Ok(Some(job)) => job,
        Ok(None) => return not_allowed(),
        Err(error) => return Err(internal_error(error)),
    };
    let target =
        match db::repo_by_path(&state.db, owner, repo.strip_suffix(".git").unwrap_or(repo)).await {
            Ok(target) => target,
            Err(error) => return Err(internal_error(error)),
        };
    let run = match db::run(&state.db, job.run_id).await {
        Ok(run) => run,
        Err(error) => return Err(internal_error(error)),
    };
    match (target, run) {
        (Some(target), Some(run)) if run.repo_id == target.id && service == Service::UploadPack => {
            Ok(Some(git_service::Grant {
                path: forge_git::repo_path(&state.repos, &target.owner_name, &target.name),
                repo_id: target.id,
                user_id: None,
            }))
        }
        _ => not_allowed(),
    }
}

fn git_protocol(headers: &HeaderMap) -> Option<String> {
    headers
        .get("git-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn git_response(content_type: &str, body: Body) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"omaforge\"")],
        "authentication required\n",
    )
        .into_response()
}

fn internal_error(error: anyhow::Error) -> Response {
    tracing::error!(%error, "internal error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
