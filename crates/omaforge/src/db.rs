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
    pub description: String,
    pub is_private: bool,
}

/// The columns of [`Repo`], for `SELECT ... FROM repos r JOIN users u ON u.id = r.owner_id`.
macro_rules! repo_columns {
    () => {
        "r.owner_id, u.username AS owner_name, r.name, r.description, r.is_private"
    };
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

/// Find a user by username or email, for signing in.
pub async fn user_by_login(db: &SqlitePool, login: &str) -> Result<Option<User>> {
    let user = sqlx::query_as(
        "SELECT id, username, password_hash, is_admin FROM users WHERE username = ?1 OR email = ?1",
    )
    .bind(login)
    .fetch_optional(db)
    .await?;
    Ok(user)
}

pub async fn set_password(db: &SqlitePool, user_id: i64, password_hash: &str) -> Result<()> {
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
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
    description: &str,
    is_private: bool,
) -> Result<i64> {
    let mut tx = db.begin().await?;
    let id = sqlx::query_scalar(
        "INSERT INTO repos (owner_id, name, description, is_private) VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(owner.id)
    .bind(name)
    .bind(description)
    .bind(is_private)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| match error {
        sqlx::Error::Database(e) if e.is_unique_violation() => {
            anyhow::anyhow!("{}/{name} already exists", owner.username)
        }
        error => error.into(),
    })?;
    forge_git::create_bare(repos_root, &owner.username, name)?;
    tx.commit().await?;
    Ok(id)
}

pub async fn repo_by_path(db: &SqlitePool, owner: &str, name: &str) -> Result<Option<Repo>> {
    let repo = sqlx::query_as(concat!(
        "SELECT ",
        repo_columns!(),
        " FROM repos r JOIN users u ON u.id = r.owner_id WHERE u.username = ? AND r.name = ?"
    ))
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

/// Repos `viewer` may see (public ones, their own, or all for admins), optionally only
/// those of one owner, alphabetically. Must agree with `auth::can_read`.
pub async fn visible_repos(
    db: &SqlitePool,
    viewer: Option<&User>,
    owner: Option<&str>,
) -> Result<Vec<Repo>> {
    let repos = sqlx::query_as(concat!(
        "SELECT ",
        repo_columns!(),
        " FROM repos r JOIN users u ON u.id = r.owner_id
         WHERE (r.is_private = 0 OR ?2 OR r.owner_id = ?3) AND (?1 IS NULL OR u.username = ?1)
         ORDER BY u.username, r.name"
    ))
    .bind(owner)
    .bind(viewer.is_some_and(|v| v.is_admin))
    .bind(viewer.map(|v| v.id))
    .fetch_all(db)
    .await?;
    Ok(repos)
}

pub async fn set_repo_description(
    db: &SqlitePool,
    owner_id: i64,
    name: &str,
    description: &str,
) -> Result<()> {
    sqlx::query("UPDATE repos SET description = ? WHERE owner_id = ? AND name = ?")
        .bind(description)
        .bind(owner_id)
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

// ---- Sessions ----

pub async fn create_session(
    db: &SqlitePool,
    user_id: i64,
    token_hash: &str,
    csrf_token: &str,
    days: u32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO sessions (user_id, token_hash, csrf_token, expires_at)
         VALUES (?, ?, ?, datetime('now', '+' || ? || ' days'))",
    )
    .bind(user_id)
    .bind(token_hash)
    .bind(csrf_token)
    .bind(days)
    .execute(db)
    .await?;
    Ok(())
}

/// The signed-in user and CSRF token for an unexpired session.
pub async fn session_user(db: &SqlitePool, token_hash: &str) -> Result<Option<(User, String)>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        user: User,
        csrf_token: String,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT u.id, u.username, u.password_hash, u.is_admin, s.csrf_token
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > datetime('now')",
    )
    .bind(token_hash)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| (row.user, row.csrf_token)))
}

pub async fn delete_session(db: &SqlitePool, token_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Sign a user out everywhere except, optionally, one session (after a password change).
pub async fn delete_user_sessions(db: &SqlitePool, user_id: i64, keep: Option<&str>) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash IS NOT ?")
        .bind(user_id)
        .bind(keep)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_expired_sessions(db: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= datetime('now')")
        .execute(db)
        .await?;
    Ok(())
}

// ---- Keys and tokens, as listed in settings ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Credential {
    pub id: i64,
    pub name: String,
    /// The SSH key fingerprint; empty for tokens.
    pub fingerprint: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

pub async fn ssh_keys(db: &SqlitePool, user_id: i64) -> Result<Vec<Credential>> {
    let keys = sqlx::query_as(
        "SELECT id, name, fingerprint, created_at, last_used_at FROM ssh_keys
         WHERE user_id = ? ORDER BY created_at DESC, id DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(keys)
}

pub async fn delete_ssh_key(db: &SqlitePool, user_id: i64, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM ssh_keys WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn tokens(db: &SqlitePool, user_id: i64) -> Result<Vec<Credential>> {
    let tokens = sqlx::query_as(
        "SELECT id, name, '' AS fingerprint, created_at, last_used_at FROM tokens
         WHERE user_id = ? ORDER BY created_at DESC, id DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(tokens)
}

pub async fn delete_token(db: &SqlitePool, user_id: i64, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM tokens WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(())
}
