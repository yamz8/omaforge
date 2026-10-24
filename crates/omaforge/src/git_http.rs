//! Git smart HTTP transport (protocol v0/v1/v2), backed by `git upload-pack` and
//! `git receive-pack` in `--stateless-rpc` mode.

use std::{path::PathBuf, process::Stdio};

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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::io::ReaderStream;
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    auth::{self, Identity},
    db, AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
        // git gzips large upload-pack requests.
        .layer(RequestDecompressionLayer::new())
}

#[derive(Clone, Copy)]
enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    fn from_query(name: &str) -> Option<Self> {
        match name {
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    fn subcommand(self) -> &'static str {
        &self.name()["git-".len()..]
    }
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
    let Some(service) = query.service.as_deref().and_then(Service::from_query) else {
        return (
            StatusCode::FORBIDDEN,
            "dumb HTTP protocol is not supported\n",
        )
            .into_response();
    };
    let path = match authorize(&state, &owner, &repo, &headers, service).await {
        Ok(path) => path,
        Err(response) => return response,
    };

    let protocol = git_protocol(&headers);
    let mut command = Command::new("git");
    command
        .args([service.subcommand(), "--stateless-rpc", "--advertise-refs"])
        .arg(&path);
    if let Some(protocol) = &protocol {
        command.env("GIT_PROTOCOL", protocol);
    }
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
    let path = match authorize(&state, &owner, &repo, &headers, service).await {
        Ok(path) => path,
        Err(response) => return response,
    };

    let mut command = Command::new("git");
    command
        .args([service.subcommand(), "--stateless-rpc"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(protocol) = git_protocol(&headers) {
        command.env("GIT_PROTOCOL", protocol);
    }
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

/// Resolve and authorize a repo for `service`, returning its on-disk path. Private repos
/// look missing to anyone who can't read them.
#[allow(clippy::result_large_err)]
async fn authorize(
    state: &AppState,
    owner: &str,
    repo: &str,
    headers: &HeaderMap,
    service: Service,
) -> Result<PathBuf, Response> {
    let name = repo.strip_suffix(".git").unwrap_or(repo);
    if !forge_git::is_valid_name(owner) || !forge_git::is_valid_name(name) {
        return Err(StatusCode::NOT_FOUND.into_response());
    }

    let user = match auth::authenticate(&state.db, headers).await {
        Ok(Identity::User(user)) => Some(user),
        Ok(Identity::Anonymous) => None,
        Ok(Identity::Invalid) => return Err(unauthorized()),
        Err(error) => return Err(internal_error(error)),
    };
    let repo = match db::repo_by_path(&state.db, owner, name).await {
        Ok(repo) => repo,
        Err(error) => return Err(internal_error(error)),
    };

    let allowed = repo.as_ref().is_some_and(|repo| match service {
        Service::UploadPack => auth::can_read(user.as_ref(), repo),
        Service::ReceivePack => auth::can_write(user.as_ref(), repo),
    });
    match (repo, allowed, &user) {
        (Some(repo), true, _) => Ok(forge_git::repo_path(
            &state.repos,
            &repo.owner_name,
            &repo.name,
        )),
        // Ask anonymous clients to log in rather than revealing whether the repo exists.
        (_, false, None) => Err(unauthorized()),
        (Some(repo), false, Some(_)) if auth::can_read(user.as_ref(), &repo) => Err((
            StatusCode::FORBIDDEN,
            "you do not have write access to this repository\n",
        )
            .into_response()),
        _ => Err(StatusCode::NOT_FOUND.into_response()),
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
