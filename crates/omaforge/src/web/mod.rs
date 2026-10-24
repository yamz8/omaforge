//! The HTML interface: server-rendered pages with no JavaScript required.

mod icons;
pub mod render;
mod repo;
mod theme;

pub use theme::Theme;

use askama::Template;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};

use crate::AppState;

const STYLE: &str = include_str!("../../assets/style.css");

/// Public-facing settings the pages need.
pub struct Site {
    /// Base URL of the web UI and HTTP git, without a trailing slash.
    pub http_url: String,
    /// Host and port for SSH clone URLs.
    pub ssh: Option<(String, u16)>,
    pub theme: Theme,
}

impl Site {
    pub fn http_clone_url(&self, owner: &str, name: &str) -> String {
        format!("{}/{owner}/{name}.git", self.http_url)
    }

    pub fn ssh_clone_url(&self, owner: &str, name: &str) -> Option<String> {
        let (host, port) = self.ssh.as_ref()?;
        Some(if *port == 22 {
            format!("git@{host}:{owner}/{name}.git")
        } else {
            format!("ssh://git@{host}:{port}/{owner}/{name}.git")
        })
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(repo::index))
        .route("/assets/style.css", get(style))
        .route("/assets/theme.css", get(theme_css))
        .route("/{owner}", get(repo::owner))
        .route("/{owner}/{repo}", get(repo::home))
        .route("/{owner}/{repo}/tree/{*rest}", get(repo::tree))
        .route("/{owner}/{repo}/blob/{*rest}", get(repo::blob))
        .route("/{owner}/{repo}/raw/{*rest}", get(repo::raw))
        .route("/{owner}/{repo}/commits", get(repo::commits_default))
        .route("/{owner}/{repo}/commits/{*rest}", get(repo::commits))
        .route("/{owner}/{repo}/commit/{id}", get(repo::commit))
        .route("/{owner}/{repo}/branches", get(repo::branches))
        .route("/{owner}/{repo}/tags", get(repo::tags))
        .fallback(|| async { WebError::NotFound })
}

async fn style() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        STYLE,
    )
}

async fn theme_css(State(state): State<AppState>) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        state.site.theme.css(),
    )
}

pub enum WebError {
    NotFound,
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for WebError {
    fn from(error: E) -> Self {
        Self::Internal(error.into())
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    status: u16,
    message: &'static str,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "This page doesn't exist, or you don't have access to it.",
            ),
            Self::Internal(error) => {
                tracing::error!(error = format!("{error:#}"), "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong on our end.",
                )
            }
        };
        let page = ErrorPage {
            status: status.as_u16(),
            message,
        };
        (
            status,
            Html(page.render().unwrap_or_else(|_| message.to_owned())),
        )
            .into_response()
    }
}

pub type WebResult<T = Response> = Result<T, WebError>;

fn page(template: impl Template) -> WebResult {
    Ok(Html(template.render()?).into_response())
}
