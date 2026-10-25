//! Webhooks and their deliveries. Pending deliveries are the delivery queue.

use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub secret: String,
    pub events: String,
    pub active: bool,
}

impl Webhook {
    pub fn wants(&self, event: &str) -> bool {
        self.active
            && (event == "ping"
                || self
                    .events
                    .split(',')
                    .any(|e| e.trim() == "*" || e.trim() == event))
    }
}

pub async fn webhooks(db: &SqlitePool, repo_id: i64) -> Result<Vec<Webhook>> {
    let hooks = sqlx::query_as(
        "SELECT id, url, secret, events, active FROM webhooks WHERE repo_id = ? ORDER BY id",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    Ok(hooks)
}

pub async fn add_webhook(
    db: &SqlitePool,
    repo_id: i64,
    url: &str,
    secret: &str,
    events: &str,
) -> Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO webhooks (repo_id, url, secret, events) VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(repo_id)
    .bind(url)
    .bind(secret)
    .bind(events)
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub async fn delete_webhook(db: &SqlitePool, repo_id: i64, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM webhooks WHERE id = ? AND repo_id = ?")
        .bind(id)
        .bind(repo_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn enqueue_delivery(
    db: &SqlitePool,
    webhook_id: i64,
    event: &str,
    guid: &str,
    payload: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO deliveries (webhook_id, event, guid, payload) VALUES (?, ?, ?, ?)")
        .bind(webhook_id)
        .bind(event)
        .bind(guid)
        .bind(payload)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Delivery {
    pub id: i64,
    pub webhook_id: i64,
    pub event: String,
    pub guid: String,
    pub payload: String,
    pub status: String,
    pub attempts: i64,
    pub response_status: Option<i64>,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    pub created: i64,
}

macro_rules! delivery_columns {
    () => {
        "d.id, d.webhook_id, d.event, d.guid, d.payload, d.status, d.attempts, d.response_status,
         d.response_body, d.error, d.duration_ms, CAST(strftime('%s', d.created_at) AS INTEGER) AS created"
    };
}

/// Pending deliveries whose time has come, with their webhook.
pub async fn due_deliveries(db: &SqlitePool, limit: i64) -> Result<Vec<(Delivery, Webhook)>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        delivery: Delivery,
        url: String,
        secret: String,
        events: String,
        active: bool,
    }
    let rows: Vec<Row> = sqlx::query_as(concat!(
        "SELECT ",
        delivery_columns!(),
        ", w.url, w.secret, w.events, w.active
         FROM deliveries d JOIN webhooks w ON w.id = d.webhook_id
         WHERE d.status = 'pending' AND d.next_attempt_at <= datetime('now')
         ORDER BY d.id LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let hook = Webhook {
                id: r.delivery.webhook_id,
                url: r.url,
                secret: r.secret,
                events: r.events,
                active: r.active,
            };
            (r.delivery, hook)
        })
        .collect())
}

pub struct Attempt<'a> {
    pub response_status: Option<i64>,
    pub response_body: Option<&'a str>,
    pub error: Option<&'a str>,
    pub duration_ms: i64,
}

/// Record a delivery attempt: done when `succeeded`, otherwise retried in `retry_in`
/// seconds, or failed for good when that's `None`.
pub async fn record_attempt(
    db: &SqlitePool,
    id: i64,
    attempt: &Attempt<'_>,
    succeeded: bool,
    retry_in: Option<i64>,
) -> Result<()> {
    let status = if succeeded {
        "succeeded"
    } else if retry_in.is_some() {
        "pending"
    } else {
        "failed"
    };
    sqlx::query(
        "UPDATE deliveries SET status = ?, attempts = attempts + 1, response_status = ?, response_body = ?,
                error = ?, duration_ms = ?, delivered_at = CURRENT_TIMESTAMP,
                next_attempt_at = datetime('now', '+' || ? || ' seconds')
         WHERE id = ?",
    )
    .bind(status)
    .bind(attempt.response_status)
    .bind(attempt.response_body)
    .bind(attempt.error)
    .bind(attempt.duration_ms)
    .bind(retry_in.unwrap_or(0))
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn recent_deliveries(
    db: &SqlitePool,
    webhook_id: i64,
    limit: i64,
) -> Result<Vec<Delivery>> {
    let deliveries = sqlx::query_as(concat!(
        "SELECT ",
        delivery_columns!(),
        " FROM deliveries d WHERE d.webhook_id = ? ORDER BY d.id DESC LIMIT ?"
    ))
    .bind(webhook_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(deliveries)
}

/// Queue a past delivery again (same event and payload, new delivery id).
pub async fn redeliver(
    db: &SqlitePool,
    repo_id: i64,
    delivery_id: i64,
    guid: &str,
) -> Result<bool> {
    let done = sqlx::query(
        "INSERT INTO deliveries (webhook_id, event, guid, payload)
         SELECT d.webhook_id, d.event, ?, d.payload FROM deliveries d JOIN webhooks w ON w.id = d.webhook_id
         WHERE d.id = ? AND w.repo_id = ?",
    )
    .bind(guid)
    .bind(delivery_id)
    .bind(repo_id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(done > 0)
}
