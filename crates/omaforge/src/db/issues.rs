//! Issues, labels and assignees. Comments, events and state changes are shared with pull
//! requests (see `pulls.rs`).

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use sqlx::SqlitePool;

/// The labels every new repository starts with.
pub const DEFAULT_LABELS: &[(&str, &str, &str)] = &[
    ("bug", "#d73a4a", "Something isn't working"),
    ("enhancement", "#a2eeef", "New feature or request"),
    (
        "documentation",
        "#0075ca",
        "Improvements or additions to documentation",
    ),
    ("question", "#d876e3", "Further information is requested"),
    ("good first issue", "#7057ff", "Good for newcomers"),
    (
        "duplicate",
        "#cfd3d7",
        "This issue or pull request already exists",
    ),
    ("wontfix", "#ffffff", "This will not be worked on"),
];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Issue {
    pub issue_id: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub author_id: i64,
    pub author: String,
    pub created: i64,
    pub comment_count: i64,
}

impl Issue {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

macro_rules! issue_select {
    ($where:literal) => {
        concat!(
            "SELECT i.id AS issue_id, i.number, i.title, i.body, i.state, i.author_id,
                    a.username AS author, CAST(strftime('%s', i.created_at) AS INTEGER) AS created,
                    (SELECT COUNT(*) FROM comments c WHERE c.issue_id = i.id) AS comment_count
             FROM issues i JOIN users a ON a.id = i.author_id
             WHERE i.is_pull = 0 AND ",
            $where
        )
    };
}

/// Open an issue, returning its number.
pub async fn create_issue(
    db: &SqlitePool,
    repo_id: i64,
    author_id: i64,
    title: &str,
    body: &str,
) -> Result<i64> {
    let number = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, author_id, title, body)
         VALUES (?1, (SELECT COALESCE(MAX(number), 0) + 1 FROM issues WHERE repo_id = ?1), ?2, ?3, ?4)
         RETURNING number",
    )
    .bind(repo_id)
    .bind(author_id)
    .bind(title)
    .bind(body)
    .fetch_one(db)
    .await?;
    Ok(number)
}

pub async fn issue(db: &SqlitePool, repo_id: i64, number: i64) -> Result<Option<Issue>> {
    let issue = sqlx::query_as(issue_select!("i.repo_id = ? AND i.number = ?"))
        .bind(repo_id)
        .bind(number)
        .fetch_optional(db)
        .await?;
    Ok(issue)
}

/// Whether `#number` is a pull request (`Some(true)`), an issue (`Some(false)`) or nothing.
pub async fn number_kind(db: &SqlitePool, repo_id: i64, number: i64) -> Result<Option<bool>> {
    let is_pull = sqlx::query_scalar("SELECT is_pull FROM issues WHERE repo_id = ? AND number = ?")
        .bind(repo_id)
        .bind(number)
        .fetch_optional(db)
        .await?;
    Ok(is_pull)
}

/// Issues that are `open` or `closed`, newest first, optionally with a label and a word
/// in the title.
pub async fn issues(
    db: &SqlitePool,
    repo_id: i64,
    state: &str,
    label: Option<&str>,
    search: Option<&str>,
) -> Result<Vec<Issue>> {
    let issues = sqlx::query_as(concat!(
        issue_select!(
            "i.repo_id = ?1 AND i.state = ?2
             AND (?3 IS NULL OR EXISTS (SELECT 1 FROM issue_labels il JOIN labels l ON l.id = il.label_id
                                        WHERE il.issue_id = i.id AND l.name = ?3))
             AND (?4 IS NULL OR i.title LIKE '%' || ?4 || '%' ESCAPE '\\')"
        ),
        " ORDER BY i.number DESC"
    ))
    .bind(repo_id)
    .bind(state)
    .bind(label)
    .bind(search.map(|s| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")))
    .fetch_all(db)
    .await?;
    Ok(issues)
}

/// `(open, closed)` issue counts.
pub async fn issue_counts(db: &SqlitePool, repo_id: i64) -> Result<(i64, i64)> {
    let counts = sqlx::query_as(
        "SELECT COALESCE(SUM(state = 'open'), 0), COALESCE(SUM(state = 'closed'), 0)
         FROM issues WHERE repo_id = ? AND is_pull = 0",
    )
    .bind(repo_id)
    .fetch_one(db)
    .await?;
    Ok(counts)
}

/// Every issue and pull request number in a repository, for linking `#123`.
pub async fn issue_numbers(db: &SqlitePool, repo_id: i64) -> Result<HashSet<i64>> {
    let numbers: Vec<i64> = sqlx::query_scalar("SELECT number FROM issues WHERE repo_id = ?")
        .bind(repo_id)
        .fetch_all(db)
        .await?;
    Ok(numbers.into_iter().collect())
}

/// Close an issue because a pull request that said "fixes #N" was merged.
pub async fn close_issue_by(
    db: &SqlitePool,
    issue_id: i64,
    actor_id: i64,
    pull_number: i64,
) -> Result<()> {
    let mut tx = db.begin().await?;
    let changed = sqlx::query(
        "UPDATE issues SET state = 'closed', updated_at = CURRENT_TIMESTAMP, closed_at = CURRENT_TIMESTAMP
         WHERE id = ? AND state = 'open'",
    )
    .bind(issue_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        sqlx::query("INSERT INTO issue_events (issue_id, actor_id, kind, detail) VALUES (?, ?, 'closed_by', ?)")
            .bind(issue_id)
            .bind(actor_id)
            .bind(pull_number.to_string())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ---- Labels ----

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub description: String,
    pub open_count: i64,
}

pub async fn labels(db: &SqlitePool, repo_id: i64) -> Result<Vec<Label>> {
    let labels = sqlx::query_as(
        "SELECT l.id, l.name, l.color, l.description,
                (SELECT COUNT(*) FROM issue_labels il JOIN issues i ON i.id = il.issue_id
                 WHERE il.label_id = l.id AND i.state = 'open') AS open_count
         FROM labels l WHERE l.repo_id = ? ORDER BY l.name",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    Ok(labels)
}

pub fn is_valid_label(name: &str, color: &str) -> bool {
    let name = name.trim();
    (1..=50).contains(&name.chars().count())
        && !name.chars().any(char::is_control)
        && color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub async fn create_label(
    db: &SqlitePool,
    repo_id: i64,
    name: &str,
    color: &str,
    description: &str,
) -> Result<()> {
    if !is_valid_label(name, color) {
        bail!("invalid label");
    }
    sqlx::query("INSERT INTO labels (repo_id, name, color, description) VALUES (?, ?, ?, ?)")
        .bind(repo_id)
        .bind(name.trim())
        .bind(color.to_ascii_lowercase())
        .bind(description.trim())
        .execute(db)
        .await
        .map_err(|error| match error {
            sqlx::Error::Database(e) if e.is_unique_violation() => {
                anyhow::anyhow!("a label named {:?} already exists", name.trim())
            }
            error => error.into(),
        })?;
    Ok(())
}

pub async fn create_default_labels(db: &SqlitePool, repo_id: i64) -> Result<()> {
    for (name, color, description) in DEFAULT_LABELS {
        sqlx::query(
            "INSERT OR IGNORE INTO labels (repo_id, name, color, description) VALUES (?, ?, ?, ?)",
        )
        .bind(repo_id)
        .bind(name)
        .bind(color)
        .bind(description)
        .execute(db)
        .await?;
    }
    Ok(())
}

pub async fn delete_label(db: &SqlitePool, repo_id: i64, label_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM labels WHERE id = ? AND repo_id = ?")
        .bind(label_id)
        .bind(repo_id)
        .execute(db)
        .await?;
    Ok(())
}

/// Labels of every issue in a repository, by issue id.
pub async fn labels_by_issue(db: &SqlitePool, repo_id: i64) -> Result<HashMap<i64, Vec<Label>>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        issue_id: i64,
        #[sqlx(flatten)]
        label: Label,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT il.issue_id, l.id, l.name, l.color, l.description, 0 AS open_count
         FROM issue_labels il JOIN labels l ON l.id = il.label_id
         WHERE l.repo_id = ? ORDER BY l.name",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    let mut map: HashMap<i64, Vec<Label>> = HashMap::new();
    for row in rows {
        map.entry(row.issue_id).or_default().push(row.label);
    }
    Ok(map)
}

/// Replace an issue's labels with `label_ids` (only labels of `repo_id` count), recording
/// what was added and removed.
pub async fn set_issue_labels(
    db: &SqlitePool,
    repo_id: i64,
    issue_id: i64,
    actor_id: i64,
    label_ids: &[i64],
) -> Result<()> {
    let repo_labels: HashMap<i64, String> = labels(db, repo_id)
        .await?
        .into_iter()
        .map(|l| (l.id, l.name))
        .collect();
    let wanted: HashSet<i64> = label_ids
        .iter()
        .copied()
        .filter(|id| repo_labels.contains_key(id))
        .collect();
    let current: HashSet<i64> =
        sqlx::query_scalar::<_, i64>("SELECT label_id FROM issue_labels WHERE issue_id = ?")
            .bind(issue_id)
            .fetch_all(db)
            .await?
            .into_iter()
            .collect();
    let mut tx = db.begin().await?;
    for id in wanted.difference(&current) {
        sqlx::query("INSERT INTO issue_labels (issue_id, label_id) VALUES (?, ?)")
            .bind(issue_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        event(&mut tx, issue_id, actor_id, "labeled", &repo_labels[id]).await?;
    }
    for id in current.difference(&wanted) {
        sqlx::query("DELETE FROM issue_labels WHERE issue_id = ? AND label_id = ?")
            .bind(issue_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if let Some(name) = repo_labels.get(id) {
            event(&mut tx, issue_id, actor_id, "unlabeled", name).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

// ---- Assignees ----

pub async fn assignees(db: &SqlitePool, issue_id: i64) -> Result<Vec<(i64, String)>> {
    let assignees = sqlx::query_as(
        "SELECT u.id, u.username FROM issue_assignees a JOIN users u ON u.id = a.user_id
         WHERE a.issue_id = ? ORDER BY u.username",
    )
    .bind(issue_id)
    .fetch_all(db)
    .await?;
    Ok(assignees)
}

/// Assignees of every issue in a repository, by issue id.
pub async fn assignees_by_issue(
    db: &SqlitePool,
    repo_id: i64,
) -> Result<HashMap<i64, Vec<String>>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT a.issue_id, u.username FROM issue_assignees a
         JOIN users u ON u.id = a.user_id JOIN issues i ON i.id = a.issue_id
         WHERE i.repo_id = ? ORDER BY u.username",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    let mut map: HashMap<i64, Vec<String>> = HashMap::new();
    for (issue_id, name) in rows {
        map.entry(issue_id).or_default().push(name);
    }
    Ok(map)
}

/// Replace an issue's assignees, recording who was added and removed.
pub async fn set_assignees(
    db: &SqlitePool,
    issue_id: i64,
    actor_id: i64,
    user_ids: &[i64],
) -> Result<()> {
    let users: HashMap<i64, String> = all_users(db).await?.into_iter().collect();
    let wanted: HashSet<i64> = user_ids
        .iter()
        .copied()
        .filter(|id| users.contains_key(id))
        .collect();
    let current: HashSet<i64> = assignees(db, issue_id)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut tx = db.begin().await?;
    for id in wanted.difference(&current) {
        sqlx::query("INSERT INTO issue_assignees (issue_id, user_id) VALUES (?, ?)")
            .bind(issue_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        event(&mut tx, issue_id, actor_id, "assigned", &users[id]).await?;
    }
    for id in current.difference(&wanted) {
        sqlx::query("DELETE FROM issue_assignees WHERE issue_id = ? AND user_id = ?")
            .bind(issue_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        event(&mut tx, issue_id, actor_id, "unassigned", &users[id]).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Everyone on this instance, `(id, username)`, for assignee pickers and `@mentions`.
pub async fn all_users(db: &SqlitePool) -> Result<Vec<(i64, String)>> {
    let users = sqlx::query_as("SELECT id, username FROM users ORDER BY username")
        .fetch_all(db)
        .await?;
    Ok(users)
}

async fn event(
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
