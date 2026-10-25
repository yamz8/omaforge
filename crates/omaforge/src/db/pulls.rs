//! Pull requests and their conversations: comments, reviews and timeline events.

use anyhow::Result;
use sqlx::SqlitePool;

/// Seconds since the epoch, from SQLite's `CURRENT_TIMESTAMP` text.
macro_rules! unix {
    ($column:literal) => {
        concat!("CAST(strftime('%s', ", $column, ") AS INTEGER)")
    };
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Pull {
    pub issue_id: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author_id: i64,
    pub author: String,
    pub created: i64,
    pub base_branch: String,
    pub head_branch: String,
    pub base_sha: String,
    pub head_sha: String,
    pub merged: Option<i64>,
    pub merged_by: Option<String>,
    pub merge_sha: Option<String>,
    pub comment_count: i64,
}

impl Pull {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    pub fn is_merged(&self) -> bool {
        self.merged.is_some()
    }
}

macro_rules! pull_select {
    ($where:literal) => {
        concat!(
            "SELECT i.id AS issue_id, i.number, i.title, i.body, i.state, i.author_id,
                    a.username AS author, ",
            unix!("i.created_at"),
            " AS created, p.base_branch, p.head_branch, p.base_sha, p.head_sha, ",
            "CASE WHEN p.merged_at IS NULL THEN NULL ELSE ",
            unix!("p.merged_at"),
            " END AS merged, m.username AS merged_by, p.merge_sha,
                    (SELECT COUNT(*) FROM comments c WHERE c.issue_id = i.id) AS comment_count
             FROM issues i
             JOIN pull_requests p ON p.issue_id = i.id
             JOIN users a ON a.id = i.author_id
             LEFT JOIN users m ON m.id = p.merged_by
             WHERE ",
            $where
        )
    };
}

pub struct NewPull<'a> {
    pub repo_id: i64,
    pub author_id: i64,
    pub title: &'a str,
    pub body: &'a str,
    pub base_branch: &'a str,
    pub head_branch: &'a str,
    pub base_sha: &'a str,
    pub head_sha: &'a str,
}

/// Open a pull request, returning its number.
pub async fn create_pull(db: &SqlitePool, pull: &NewPull<'_>) -> Result<i64> {
    let mut tx = db.begin().await?;
    let (issue_id, number): (i64, i64) = sqlx::query_as(
        "INSERT INTO issues (repo_id, number, author_id, title, body, is_pull)
         VALUES (?1, (SELECT COALESCE(MAX(number), 0) + 1 FROM issues WHERE repo_id = ?1), ?2, ?3, ?4, 1)
         RETURNING id, number",
    )
    .bind(pull.repo_id)
    .bind(pull.author_id)
    .bind(pull.title)
    .bind(pull.body)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO pull_requests (issue_id, base_branch, head_branch, base_sha, head_sha)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(issue_id)
    .bind(pull.base_branch)
    .bind(pull.head_branch)
    .bind(pull.base_sha)
    .bind(pull.head_sha)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(number)
}

pub async fn pull(db: &SqlitePool, repo_id: i64, number: i64) -> Result<Option<Pull>> {
    let pull = sqlx::query_as(pull_select!("i.repo_id = ? AND i.number = ?"))
        .bind(repo_id)
        .bind(number)
        .fetch_optional(db)
        .await?;
    Ok(pull)
}

/// Pull requests that are `open` or `closed` (merged ones count as closed), newest first.
pub async fn pulls(db: &SqlitePool, repo_id: i64, state: &str) -> Result<Vec<Pull>> {
    let pulls = sqlx::query_as(concat!(
        pull_select!("i.repo_id = ? AND i.state = ?"),
        " ORDER BY i.number DESC"
    ))
    .bind(repo_id)
    .bind(state)
    .fetch_all(db)
    .await?;
    Ok(pulls)
}

/// `(open, closed)` pull request counts.
pub async fn pull_counts(db: &SqlitePool, repo_id: i64) -> Result<(i64, i64)> {
    let counts = sqlx::query_as(
        "SELECT COALESCE(SUM(state = 'open'), 0), COALESCE(SUM(state = 'closed'), 0)
         FROM issues WHERE repo_id = ? AND is_pull = 1",
    )
    .bind(repo_id)
    .fetch_one(db)
    .await?;
    Ok(counts)
}

/// The number of an open pull request from `head` into `base`, if there is one.
pub async fn open_pull_for(
    db: &SqlitePool,
    repo_id: i64,
    base: &str,
    head: &str,
) -> Result<Option<i64>> {
    let number = sqlx::query_scalar(
        "SELECT i.number FROM issues i JOIN pull_requests p ON p.issue_id = i.id
         WHERE i.repo_id = ? AND i.state = 'open' AND p.base_branch = ? AND p.head_branch = ?",
    )
    .bind(repo_id)
    .bind(base)
    .bind(head)
    .fetch_optional(db)
    .await?;
    Ok(number)
}

/// Remember the branches' current tips.
pub async fn update_pull_shas(
    db: &SqlitePool,
    issue_id: i64,
    base_sha: &str,
    head_sha: &str,
) -> Result<()> {
    sqlx::query("UPDATE pull_requests SET base_sha = ?, head_sha = ? WHERE issue_id = ?")
        .bind(base_sha)
        .bind(head_sha)
        .bind(issue_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_issue_state(
    db: &SqlitePool,
    issue_id: i64,
    actor_id: i64,
    open: bool,
) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE issues SET state = ?, updated_at = CURRENT_TIMESTAMP,
                closed_at = CASE WHEN ? THEN NULL ELSE CURRENT_TIMESTAMP END
         WHERE id = ?",
    )
    .bind(if open { "open" } else { "closed" })
    .bind(open)
    .bind(issue_id)
    .execute(&mut *tx)
    .await?;
    insert_event(
        &mut tx,
        issue_id,
        actor_id,
        if open { "reopened" } else { "closed" },
        "",
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub struct Merged<'a> {
    pub issue_id: i64,
    pub merged_by: i64,
    pub method: &'a str,
    pub merge_sha: &'a str,
    pub base_sha: &'a str,
    pub head_sha: &'a str,
}

pub async fn mark_merged(db: &SqlitePool, merged: &Merged<'_>) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "UPDATE pull_requests SET merged_at = CURRENT_TIMESTAMP, merged_by = ?, merge_sha = ?,
                merge_method = ?, base_sha = ?, head_sha = ?
         WHERE issue_id = ?",
    )
    .bind(merged.merged_by)
    .bind(merged.merge_sha)
    .bind(merged.method)
    .bind(merged.base_sha)
    .bind(merged.head_sha)
    .bind(merged.issue_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE issues SET state = 'closed', updated_at = CURRENT_TIMESTAMP, closed_at = CURRENT_TIMESTAMP
         WHERE id = ?",
    )
    .bind(merged.issue_id)
    .execute(&mut *tx)
    .await?;
    insert_event(
        &mut tx,
        merged.issue_id,
        merged.merged_by,
        "merged",
        merged.merge_sha,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn add_event(
    db: &SqlitePool,
    issue_id: i64,
    actor_id: i64,
    kind: &str,
    detail: &str,
) -> Result<()> {
    let mut tx = db.begin().await?;
    insert_event(&mut tx, issue_id, actor_id, kind, detail).await?;
    tx.commit().await?;
    Ok(())
}

async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    issue_id: i64,
    actor_id: i64,
    kind: &str,
    detail: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO issue_events (issue_id, actor_id, kind, detail) VALUES (?, ?, ?, ?)")
        .bind(issue_id)
        .bind(actor_id)
        .bind(kind)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Where on the diff a review comment sits.
pub struct LineAnchor<'a> {
    pub path: &'a str,
    pub line: i64,
    /// `old` or `new`.
    pub side: &'a str,
    pub commit_sha: &'a str,
}

pub async fn add_comment(
    db: &SqlitePool,
    issue_id: i64,
    author_id: i64,
    body: &str,
    anchor: Option<&LineAnchor<'_>>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO comments (issue_id, author_id, body, path, line, side, commit_sha)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(issue_id)
    .bind(author_id)
    .bind(body)
    .bind(anchor.map(|a| a.path))
    .bind(anchor.map(|a| a.line))
    .bind(anchor.map(|a| a.side))
    .bind(anchor.map(|a| a.commit_sha))
    .execute(db)
    .await?;
    sqlx::query("UPDATE issues SET updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(issue_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn add_review(
    db: &SqlitePool,
    issue_id: i64,
    author_id: i64,
    state: &str,
    body: &str,
    commit_sha: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO reviews (issue_id, author_id, state, body, commit_sha) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(issue_id)
    .bind(author_id)
    .bind(state)
    .bind(body)
    .bind(commit_sha)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Comment {
    pub id: i64,
    pub author: String,
    pub body: String,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub side: Option<String>,
    pub commit_sha: Option<String>,
    pub created: i64,
}

pub async fn comments(db: &SqlitePool, issue_id: i64) -> Result<Vec<Comment>> {
    let comments = sqlx::query_as(concat!(
        "SELECT c.id, u.username AS author, c.body, c.path, c.line, c.side, c.commit_sha, ",
        unix!("c.created_at"),
        " AS created FROM comments c JOIN users u ON u.id = c.author_id
         WHERE c.issue_id = ? ORDER BY c.created_at, c.id"
    ))
    .bind(issue_id)
    .fetch_all(db)
    .await?;
    Ok(comments)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Review {
    pub id: i64,
    pub author: String,
    pub state: String,
    pub body: String,
    pub commit_sha: String,
    pub created: i64,
}

pub async fn reviews(db: &SqlitePool, issue_id: i64) -> Result<Vec<Review>> {
    let reviews = sqlx::query_as(concat!(
        "SELECT r.id, u.username AS author, r.state, r.body, r.commit_sha, ",
        unix!("r.created_at"),
        " AS created FROM reviews r JOIN users u ON u.id = r.author_id
         WHERE r.issue_id = ? ORDER BY r.created_at, r.id"
    ))
    .bind(issue_id)
    .fetch_all(db)
    .await?;
    Ok(reviews)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub actor: String,
    pub kind: String,
    pub detail: String,
    pub created: i64,
}

pub async fn events(db: &SqlitePool, issue_id: i64) -> Result<Vec<Event>> {
    let events = sqlx::query_as(concat!(
        "SELECT e.id, u.username AS actor, e.kind, e.detail, ",
        unix!("e.created_at"),
        " AS created FROM issue_events e JOIN users u ON u.id = e.actor_id
         WHERE e.issue_id = ? ORDER BY e.created_at, e.id"
    ))
    .bind(issue_id)
    .fetch_all(db)
    .await?;
    Ok(events)
}
