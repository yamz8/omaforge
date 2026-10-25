use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier},
    Argon2,
};
use axum::http::{header, HeaderMap};
use base64::Engine;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::db::{self, Repo, User};

const TOKEN_PREFIX: &str = "omf_";

pub fn hash_password(password: &str) -> Result<String> {
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow!("hashing password: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// Check a password for a user who may not exist, taking about as long either way so
/// response times don't reveal which usernames exist.
pub fn verify_user_password(password: &str, user: Option<&User>) -> bool {
    // A valid hash of a random password, verified against when there is no real one.
    const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$YdDtW2zR5n0XHmyiGgSOQpjzZpuNfZrfQ1WYpphKLM0";
    match user.and_then(|u| u.password_hash.as_deref()) {
        Some(hash) => verify_password(password, hash),
        None => {
            verify_password(password, DUMMY);
            false
        }
    }
}

/// 32 random bytes as hex.
pub fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("generating secret: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Generate a new personal access token, returning `(token, hash_to_store)`.
pub fn new_token() -> Result<(String, String)> {
    let token = format!("{TOKEN_PREFIX}{}", random_secret()?);
    let hash = hash_token(&token);
    Ok((token, hash))
}

/// Compare secrets without leaking where they differ through timing.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub enum Identity {
    Anonymous,
    User(User),
    /// Credentials were sent but did not match.
    Invalid,
}

/// Authenticate HTTP Basic credentials. The password may be the account password or a
/// personal access token; with a token the username is ignored, as on GitHub.
pub async fn authenticate(db: &SqlitePool, headers: &HeaderMap) -> Result<Identity> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(Identity::Anonymous);
    };
    let Some((username, secret)) = parse_basic(value.as_bytes()) else {
        return Ok(Identity::Invalid);
    };

    let user = if secret.starts_with(TOKEN_PREFIX) {
        db::user_by_token_hash(db, &hash_token(&secret)).await?
    } else {
        db::user_by_username(db, &username).await?.filter(|user| {
            user.password_hash
                .as_deref()
                .is_some_and(|hash| verify_password(&secret, hash))
        })
    };
    Ok(user.map_or(Identity::Invalid, Identity::User))
}

fn parse_basic(value: &[u8]) -> Option<(String, String)> {
    let encoded = value.strip_prefix(b"Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, secret) = decoded.split_once(':')?;
    Some((username.to_owned(), secret.to_owned()))
}

/// What someone may do with a repository, from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    None,
    /// Browse and clone.
    Read,
    /// Also push, merge pull requests, and triage issues.
    Write,
    /// Also change settings and collaborators: the owner and site admins.
    Admin,
}

/// `user`'s access to `repo`: owners and site admins administer it, collaborators get
/// their role, and everyone can read a public repository.
pub async fn access(db: &SqlitePool, user: Option<&User>, repo: &Repo) -> Result<Access> {
    let public = if repo.is_private {
        Access::None
    } else {
        Access::Read
    };
    let Some(user) = user else {
        return Ok(public);
    };
    if user.is_admin || user.id == repo.owner_id {
        return Ok(Access::Admin);
    }
    Ok(
        match db::collaborator_role(db, repo.id, user.id)
            .await?
            .as_deref()
        {
            Some("write") => Access::Write,
            Some("read") => Access::Read,
            _ => public,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_user_still_runs_argon2() {
        // If the dummy hash ever stopped parsing, unknown usernames would answer instantly.
        let start = std::time::Instant::now();
        assert!(!verify_user_password("anything", None));
        let missing = start.elapsed();

        let user = User {
            id: 1,
            username: "u".into(),
            password_hash: Some(hash_password("correct horse").unwrap()),
            is_admin: false,
        };
        let start = std::time::Instant::now();
        assert!(!verify_user_password("wrong", Some(&user)));
        let wrong = start.elapsed();
        assert!(verify_user_password("correct horse", Some(&user)));

        assert!(
            missing * 4 > wrong,
            "dummy check took {missing:?}, real check {wrong:?}"
        );
    }

    #[test]
    fn compares_secrets() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
