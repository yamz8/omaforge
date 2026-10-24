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

/// Generate a new personal access token, returning `(token, hash_to_store)`.
pub fn new_token() -> Result<(String, String)> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("generating token: {e}"))?;
    let token = format!("{TOKEN_PREFIX}{}", hex::encode(bytes));
    let hash = hash_token(&token);
    Ok((token, hash))
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

pub fn can_read(user: Option<&User>, repo: &Repo) -> bool {
    !repo.is_private || can_write(user, repo)
}

pub fn can_write(user: Option<&User>, repo: &Repo) -> bool {
    user.is_some_and(|user| user.is_admin || user.id == repo.owner_id)
}
