use std::path::Path;

use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: Option<String>,
    pub is_admin: bool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Repo {
    pub owner_id: i64,
    /// Canonical owner username, used for the on-disk path.
    pub owner_name: String,
    pub name: String,
    pub is_private: bool,
}

/// Open (creating if needed) the database in `data` and run pending migrations.
pub async fn open(data: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(data.join("omaforge.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

pub async fn create_user(
    db: &SqlitePool,
    username: &str,
    email: Option<&str>,
    password_hash: &str,
    is_admin: bool,
) -> Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash, is_admin) VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(username)
    .bind(email)
    .bind(password_hash)
    .bind(is_admin)
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub async fn user_by_username(db: &SqlitePool, username: &str) -> Result<Option<User>> {
    let user = sqlx::query_as(
        "SELECT id, username, password_hash, is_admin FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(db)
    .await?;
    Ok(user)
}

pub async fn create_token(
    db: &SqlitePool,
    user_id: i64,
    name: &str,
    token_hash: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO tokens (user_id, name, token_hash) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(name)
        .bind(token_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Look up the owner of a token and record that it was used.
pub async fn user_by_token_hash(db: &SqlitePool, token_hash: &str) -> Result<Option<User>> {
    let user: Option<User> = sqlx::query_as(
        "SELECT u.id, u.username, u.password_hash, u.is_admin
         FROM tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = ?",
    )
    .bind(token_hash)
    .fetch_optional(db)
    .await?;
    if user.is_some() {
        sqlx::query("UPDATE tokens SET last_used_at = CURRENT_TIMESTAMP WHERE token_hash = ?")
            .bind(token_hash)
            .execute(db)
            .await?;
    }
    Ok(user)
}

/// Insert a repo row and create its bare repository on disk, atomically.
pub async fn create_repo(
    db: &SqlitePool,
    repos_root: &Path,
    owner: &User,
    name: &str,
    is_private: bool,
) -> Result<i64> {
    let mut tx = db.begin().await?;
    let id = sqlx::query_scalar(
        "INSERT INTO repos (owner_id, name, is_private) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(owner.id)
    .bind(name)
    .bind(is_private)
    .fetch_one(&mut *tx)
    .await?;
    forge_git::create_bare(repos_root, &owner.username, name)?;
    tx.commit().await?;
    Ok(id)
}

pub async fn repo_by_path(db: &SqlitePool, owner: &str, name: &str) -> Result<Option<Repo>> {
    let repo = sqlx::query_as(
        "SELECT r.owner_id, u.username AS owner_name, r.name, r.is_private
         FROM repos r JOIN users u ON u.id = r.owner_id
         WHERE u.username = ? AND r.name = ?",
    )
    .bind(owner)
    .bind(name)
    .fetch_optional(db)
    .await?;
    Ok(repo)
}

pub async fn add_ssh_key(
    db: &SqlitePool,
    user_id: i64,
    name: &str,
    public_key: &str,
    fingerprint: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ssh_keys (user_id, name, public_key, fingerprint) VALUES (?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(name)
    .bind(public_key)
    .bind(fingerprint)
    .execute(db)
    .await
    .map_err(|error| match error {
        sqlx::Error::Database(e) if e.is_unique_violation() => {
            anyhow::anyhow!("SSH key {fingerprint} is already registered")
        }
        error => error.into(),
    })?;
    Ok(())
}

pub async fn user_by_ssh_fingerprint(db: &SqlitePool, fingerprint: &str) -> Result<Option<User>> {
    let user = sqlx::query_as(
        "SELECT u.id, u.username, u.password_hash, u.is_admin
         FROM ssh_keys k JOIN users u ON u.id = k.user_id
         WHERE k.fingerprint = ?",
    )
    .bind(fingerprint)
    .fetch_optional(db)
    .await?;
    Ok(user)
}

pub async fn touch_ssh_key(db: &SqlitePool, fingerprint: &str) -> Result<()> {
    sqlx::query("UPDATE ssh_keys SET last_used_at = CURRENT_TIMESTAMP WHERE fingerprint = ?")
        .bind(fingerprint)
        .execute(db)
        .await?;
    Ok(())
}
