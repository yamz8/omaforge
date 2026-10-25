//! Signing in and out of the web UI, and knowing who is viewing a page.

use askama::Template;
use axum::{
    extract::{FromRequestParts, Query, State},
    http::{header, request::Parts, HeaderMap, HeaderValue},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use serde::Deserialize;

use super::{page, WebError, WebResult};
use crate::{auth, db, AppState};

const COOKIE: &str = "omaforge_session";
const SESSION_DAYS: u32 = 30;

/// Who is viewing a page. Every page template gets one, for the header and for forms.
#[derive(Clone, Default)]
pub struct Viewer {
    pub user: Option<db::User>,
    /// Put in every form as `csrf` and checked on submit.
    pub csrf: String,
    /// Hash of the session token, to identify this session.
    session: Option<String>,
}

impl Viewer {
    /// The signed-in user, or a redirect to the sign-in page that comes back to `here`.
    pub fn require(&self, here: &str) -> WebResult<&db::User> {
        self.user
            .as_ref()
            .ok_or_else(|| WebError::SignIn(here.to_owned()))
    }

    /// Reject a form that didn't come from one of our pages.
    pub fn check_csrf(&self, token: &str) -> WebResult<()> {
        if self.user.is_some() && auth::constant_time_eq(&self.csrf, token) {
            Ok(())
        } else {
            Err(WebError::Forbidden)
        }
    }

    pub fn session_hash(&self) -> Option<&str> {
        self.session.as_deref()
    }
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, WebError> {
        let Some(token) = cookie(&parts.headers, COOKIE) else {
            return Ok(Self::default());
        };
        let hash = auth::hash_token(token);
        Ok(match db::session_user(&state.db, &hash).await? {
            Some((user, csrf)) => Self {
                user: Some(user),
                csrf,
                session: Some(hash),
            },
            None => Self::default(),
        })
    }
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name && !value.is_empty()).then_some(value)
        })
}

fn session_cookie(state: &AppState, value: &str, max_age: u64) -> HeaderValue {
    let secure = if state.site.http_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie =
        format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}");
    HeaderValue::from_str(&cookie).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Only redirect back to paths on this site, never to another host (`//evil.example`).
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(next)
            if next.starts_with('/') && !next.starts_with("//") && !next.starts_with("/\\") =>
        {
            next.to_owned()
        }
        _ => "/".to_owned(),
    }
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    viewer: Viewer,
    next: String,
    login: String,
    error: Option<&'static str>,
}

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

pub async fn login_form(viewer: Viewer, Query(query): Query<NextQuery>) -> WebResult {
    let next = safe_next(query.next.as_deref());
    if viewer.user.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    page(LoginPage {
        viewer,
        next,
        login: String::new(),
        error: None,
    })
}

#[derive(Deserialize)]
pub struct LoginForm {
    login: String,
    password: String,
    next: Option<String>,
}

pub async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> WebResult {
    let next = safe_next(form.next.as_deref());
    let login = form.login.trim();
    let user = db::user_by_login(&state.db, login).await?;
    // Argon2 is slow, so this runs off the async workers.
    let password = form.password;
    let checked = user.clone();
    let valid = tokio::task::spawn_blocking(move || {
        auth::verify_user_password(&password, checked.as_ref())
    })
    .await
    .map_err(anyhow::Error::from)?;
    let Some(user) = user.filter(|_| valid) else {
        tracing::info!(login, "failed sign-in");
        return page(LoginPage {
            viewer: Viewer::default(),
            next,
            login: login.to_owned(),
            error: Some("Incorrect username or password."),
        });
    };

    let token = auth::random_secret()?;
    let csrf = auth::random_secret()?;
    db::create_session(
        &state.db,
        user.id,
        &auth::hash_token(&token),
        &csrf,
        SESSION_DAYS,
    )
    .await?;
    db::delete_expired_sessions(&state.db).await?;
    tracing::info!(user = %user.username, "signed in");

    let mut response = Redirect::to(&next).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&state, &token, u64::from(SESSION_DAYS) * 86400),
    );
    Ok(response)
}

#[derive(Deserialize)]
pub struct CsrfForm {
    csrf: String,
}

impl CsrfForm {
    pub fn csrf(&self) -> &str {
        &self.csrf
    }
}

pub async fn logout(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(form): Form<CsrfForm>,
) -> WebResult {
    viewer.check_csrf(&form.csrf)?;
    if let Some(hash) = viewer.session_hash() {
        db::delete_session(&state.db, hash).await?;
    }
    let mut response: Response = Redirect::to("/").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&state, "", 0));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_stay_on_site() {
        assert_eq!(safe_next(Some("/alice/hello")), "/alice/hello");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("/\\evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(None), "/");
    }

    #[test]
    fn reads_cookies() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; omaforge_session=abc; b=2"),
        );
        assert_eq!(cookie(&headers, COOKIE), Some("abc"));
        assert_eq!(cookie(&headers, "missing"), None);
    }
}
