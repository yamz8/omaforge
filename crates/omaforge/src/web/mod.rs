//! The HTML interface: server-rendered pages with no JavaScript required.

mod conversation;
mod diff;
mod icons;
mod issues;
mod pulls;
pub mod render;
mod repo;
mod session;
mod settings;
mod theme;

pub use session::Viewer;
pub use theme::Theme;

use askama::Template;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};

use crate::AppState;

const STYLE: &str = include_str!("../../assets/style.css");

/// The stylesheet's URL, versioned by content so it can be cached forever and still
/// update the moment omaforge does.
pub fn style_url() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let hash = hex::encode(Sha256::digest(STYLE.as_bytes()));
        format!("/assets/style.css?v={}", &hash[..12])
    })
}

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
        .route("/login", get(session::login_form).post(session::login))
        .route("/logout", post(session::logout))
        .route(
            "/new",
            get(settings::new_repo_form).post(settings::create_repo),
        )
        .route("/settings", get(settings::index))
        .route(
            "/settings/keys",
            get(settings::keys).post(settings::add_key),
        )
        .route("/settings/keys/{id}/delete", post(settings::delete_key))
        .route(
            "/settings/tokens",
            get(settings::tokens).post(settings::create_token),
        )
        .route("/settings/tokens/{id}/delete", post(settings::delete_token))
        .route(
            "/settings/password",
            get(settings::password).post(settings::change_password),
        )
        .route("/{owner}", get(repo::owner))
        .route("/{owner}/{repo}", get(repo::home))
        .route("/{owner}/{repo}/tree/{*rest}", get(repo::tree))
        .route("/{owner}/{repo}/blob/{*rest}", get(repo::blob))
        .route("/{owner}/{repo}/raw/{*rest}", get(repo::raw))
        .route("/{owner}/{repo}/commits", get(repo::commits_default))
        .route("/{owner}/{repo}/commits/{*rest}", get(repo::commits))
        .route("/{owner}/{repo}/commit/{id}", get(repo::commit))
        .route(
            "/{owner}/{repo}/issues",
            get(issues::list).post(issues::create),
        )
        .route("/{owner}/{repo}/issues/new", get(issues::new_form))
        .route("/{owner}/{repo}/issues/{number}", get(issues::show))
        .route(
            "/{owner}/{repo}/issues/{number}/comment",
            post(issues::comment),
        )
        .route(
            "/{owner}/{repo}/issues/{number}/labels",
            post(issues::set_labels),
        )
        .route(
            "/{owner}/{repo}/issues/{number}/assignees",
            post(issues::set_assignees),
        )
        .route(
            "/{owner}/{repo}/labels",
            get(issues::labels).post(issues::create_label),
        )
        .route(
            "/{owner}/{repo}/labels/{id}/delete",
            post(issues::delete_label),
        )
        .route("/{owner}/{repo}/pulls", get(pulls::list).post(pulls::open))
        .route("/{owner}/{repo}/compare", get(pulls::compare))
        .route("/{owner}/{repo}/pull/{number}", get(pulls::conversation))
        .route("/{owner}/{repo}/pull/{number}/commits", get(pulls::commits))
        .route("/{owner}/{repo}/pull/{number}/files", get(pulls::files))
        .route(
            "/{owner}/{repo}/pull/{number}/comment",
            post(pulls::comment),
        )
        .route("/{owner}/{repo}/pull/{number}/review", post(pulls::review))
        .route("/{owner}/{repo}/pull/{number}/merge", post(pulls::merge))
        .route(
            "/{owner}/{repo}/pull/{number}/state",
            post(pulls::set_state),
        )
        .route(
            "/{owner}/{repo}/pull/{number}/delete-branch",
            post(pulls::delete_branch),
        )
        .route("/{owner}/{repo}/branches", get(repo::branches))
        .route("/{owner}/{repo}/tags", get(repo::tags))
        .fallback(|| async { WebError::NotFound })
}

async fn style() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
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
    /// Needs a signed-in user; sign in and come back to this path.
    SignIn(String),
    /// A form without a valid CSRF token.
    Forbidden,
    /// The thing lives at another URL (e.g. `/pull/3` when #3 is an issue).
    Redirect(String),
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
    viewer: Viewer,
    status: u16,
    message: &'static str,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::SignIn(next) => {
                let next: String = form_urlencode(&next);
                return Redirect::to(&format!("/login?next={next}")).into_response();
            }
            Self::Redirect(url) => return Redirect::permanent(&url).into_response(),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "This form expired or didn't come from omaforge. Go back, reload the page and try again.",
            ),
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
            viewer: Viewer::default(),
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

/// Encode a value for a query string.
pub(crate) fn form_urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The host part of a URL like `https://forge.example.com:8443/path`.
pub fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or(rest);
    match authority.strip_prefix('[') {
        // [::1]:3000
        Some(v6) => format!("[{}]", v6.split(']').next().unwrap_or(v6)),
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
            .to_owned(),
    }
}
