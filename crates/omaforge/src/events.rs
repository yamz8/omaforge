//! Events — pushes, pull requests, issues, comments, reviews — sent to webhooks as
//! GitHub-shaped payloads, so tools written for GitHub's webhooks work unchanged.
//!
//! Emitting an event queues a delivery for each webhook that wants it; a background
//! worker sends them, signed with the webhook's secret, and retries failures with
//! backoff. Emitting never fails the request that caused it.

use std::{sync::OnceLock, time::Duration};

use anyhow::{Context, Result};
use forge_git::Repository;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::Notify;

use crate::{api, auth::Access, db, hooks::RefUpdate, web, AppState};

/// Attempts before a delivery is marked failed, and the waits between them.
const RETRY_DELAYS: &[i64] = &[10, 60, 300, 1800];
const COMMIT_LIMIT: usize = 20;

/// Wakes the worker when this process queues a delivery (the hook process can't, so
/// the worker also polls).
fn wakeup() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

pub fn guid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok();
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Queue `payload` for every webhook of `repo` that wants `event`.
pub async fn emit(state: &AppState, repo: &db::Repo, event: &str, payload: &Value) -> Result<()> {
    let body = payload.to_string();
    let mut queued = false;
    for hook in db::webhooks(&state.db, repo.id).await? {
        if hook.wants(event) && event != "ping" {
            db::enqueue_delivery(&state.db, hook.id, event, &guid(), &body).await?;
            queued = true;
        }
    }
    if queued {
        wakeup().notify_one();
    }
    Ok(())
}

/// Log instead of failing: events are a side effect of something that already happened.
async fn quietly(what: &str, result: impl std::future::Future<Output = Result<()>>) {
    if let Err(error) = result.await {
        tracing::warn!(error = format!("{error:#}"), "couldn't record {what} event");
    }
}

pub fn repository(state: &AppState, repo: &db::Repo) -> Value {
    let mut value = api::repo_json(state, repo, Access::Read, "");
    // Permissions describe the viewer, which a webhook doesn't have.
    if let Some(object) = value.as_object_mut() {
        object.remove("permissions");
        object.remove("default_branch");
    }
    value
}

fn sender(state: &AppState, user: &db::User) -> Value {
    api::user_json(state, user.id, &user.username)
}

// ---- Pushes ----

/// A push received through omaforge (called from the post-receive hook).
pub async fn pushed(
    state: &AppState,
    repo_id: i64,
    pusher_id: Option<i64>,
    updates: &[RefUpdate],
) -> Result<()> {
    let repo = db::repo_by_id(&state.db, repo_id)
        .await?
        .context("repository not found")?;
    let pusher = match pusher_id {
        Some(id) => db::user_by_id(&state.db, id).await?,
        None => None,
    };
    let Some(pusher) = pusher else { return Ok(()) };
    for update in updates {
        push(state, &repo, &pusher, update).await?;
    }
    Ok(())
}

/// Record one ref changing: `push`, plus `create`/`delete`, plus `synchronize` for open
/// pull requests from that branch.
pub async fn push(
    state: &AppState,
    repo: &db::Repo,
    pusher: &db::User,
    update: &RefUpdate,
) -> Result<()> {
    let git = Repository::open(forge_git::repo_path(
        &state.repos,
        &repo.owner_name,
        &repo.name,
    ));
    let repo_url = format!("{}/{}/{}", state.site.http_url, repo.owner_name, repo.name);
    let forced = !update.is_create()
        && !update.is_delete()
        && !git
            .is_ancestor(&update.old, &update.new)
            .await
            .unwrap_or(true);
    let commits = if update.is_delete() {
        Vec::new()
    } else if update.is_create() {
        git.commit(&update.new).await?.into_iter().collect()
    } else {
        git.compare_log(&update.old, &update.new, COMMIT_LIMIT)
            .await?
    };
    let commit_json = |c: &forge_git::Commit| {
        json!({
            "id": c.id,
            "message": if c.body.is_empty() { c.subject.clone() } else { format!("{}\n\n{}", c.subject, c.body) },
            "timestamp": web::render::iso8601(c.author_time),
            "url": format!("{repo_url}/commit/{}", c.id),
            "author": { "name": c.author_name, "email": c.author_email },
            "committer": { "name": c.committer_name },
            "distinct": true,
        })
    };
    let head_commit = if update.is_delete() {
        Value::Null
    } else {
        git.commit(&update.new)
            .await?
            .as_ref()
            .map_or(Value::Null, commit_json)
    };
    let email = db::user_email(&state.db, pusher.id).await?;
    let payload = json!({
        "ref": update.name,
        "before": update.old,
        "after": update.new,
        "created": update.is_create(),
        "deleted": update.is_delete(),
        "forced": forced,
        "base_ref": null,
        "compare": format!("{repo_url}/compare?base={}&head={}", &update.old[..12.min(update.old.len())], &update.new[..12.min(update.new.len())]),
        "commits": commits.iter().map(commit_json).collect::<Vec<_>>(),
        "head_commit": head_commit,
        "repository": repository(state, repo),
        "pusher": { "name": pusher.username, "email": email },
        "sender": sender(state, pusher),
    });
    emit(state, repo, "push", &payload).await?;
    crate::ci::on_event(state, repo, "push", &payload, pusher).await;

    let (short, ref_type) = match (
        update.name.strip_prefix("refs/heads/"),
        update.name.strip_prefix("refs/tags/"),
    ) {
        (Some(branch), _) => (branch, "branch"),
        (_, Some(tag)) => (tag, "tag"),
        _ => return Ok(()),
    };
    if update.is_create() || update.is_delete() {
        let event = if update.is_create() {
            "create"
        } else {
            "delete"
        };
        let payload = json!({
            "ref": short,
            "ref_type": ref_type,
            "repository": repository(state, repo),
            "sender": sender(state, pusher),
        });
        emit(state, repo, event, &payload).await?;
    }
    if ref_type == "branch" && !update.is_delete() {
        for pull in db::pulls(&state.db, repo.id, "open").await? {
            if pull.head_branch == short {
                pull_request_payload(state, repo, &pull, "synchronize", pusher).await?;
            }
        }
    }
    Ok(())
}

// ---- Pull requests, issues, comments, reviews ----

fn pull_json(state: &AppState, repo: &db::Repo, pull: &db::Pull) -> Value {
    let html = format!(
        "{}/{}/{}/pull/{}",
        state.site.http_url, repo.owner_name, repo.name, pull.number
    );
    json!({
        "number": pull.number,
        "node_id": format!("PR_{}", pull.issue_id),
        "html_url": html,
        "state": if pull.is_open() { "open" } else { "closed" },
        "title": pull.title,
        "body": pull.body,
        "user": api::user_json(state, pull.author_id, &pull.author),
        "created_at": web::render::iso8601(pull.created),
        "merged": pull.is_merged(),
        "merged_at": pull.merged.map(web::render::iso8601),
        "merge_commit_sha": pull.merge_sha,
        "head": { "ref": pull.head_branch, "sha": pull.head_sha, "repo": repository(state, repo) },
        "base": { "ref": pull.base_branch, "sha": pull.base_sha, "repo": repository(state, repo) },
        "comments": pull.comment_count,
    })
}

async fn pull_request_payload(
    state: &AppState,
    repo: &db::Repo,
    pull: &db::Pull,
    action: &str,
    actor: &db::User,
) -> Result<()> {
    let payload = json!({
        "action": action,
        "number": pull.number,
        "pull_request": pull_json(state, repo, pull),
        "repository": repository(state, repo),
        "sender": sender(state, actor),
    });
    emit(state, repo, "pull_request", &payload).await?;
    crate::ci::on_event(state, repo, "pull_request", &payload, actor).await;
    Ok(())
}

/// `opened`, `closed` (with `merged` true or false), `reopened`, `edited`, ...
pub async fn pull_request(
    state: &AppState,
    repo: &db::Repo,
    number: i64,
    action: &str,
    actor: &db::User,
) {
    quietly("pull_request", async {
        let Some(pull) = db::pull(&state.db, repo.id, number).await? else {
            return Ok(());
        };
        pull_request_payload(state, repo, &pull, action, actor).await
    })
    .await;
}

pub async fn review(
    state: &AppState,
    repo: &db::Repo,
    number: i64,
    review_state: &str,
    body: &str,
    actor: &db::User,
) {
    quietly("pull_request_review", async {
        let Some(pull) = db::pull(&state.db, repo.id, number).await? else { return Ok(()) };
        let payload = json!({
            "action": "submitted",
            "review": {
                "state": review_state,
                "body": body,
                "user": sender(state, actor),
                "html_url": format!("{}/{}/{}/pull/{}", state.site.http_url, repo.owner_name, repo.name, number),
            },
            "pull_request": pull_json(state, repo, &pull),
            "repository": repository(state, repo),
            "sender": sender(state, actor),
        });
        emit(state, repo, "pull_request_review", &payload).await
    })
    .await;
}

fn issue_json(state: &AppState, repo: &db::Repo, issue: &db::Issue, labels: &[db::Label]) -> Value {
    json!({
        "number": issue.number,
        "node_id": format!("I_{}", issue.issue_id),
        "html_url": format!("{}/{}/{}/issues/{}", state.site.http_url, repo.owner_name, repo.name, issue.number),
        "state": if issue.is_open() { "open" } else { "closed" },
        "title": issue.title,
        "body": issue.body,
        "user": api::user_json(state, issue.author_id, &issue.author),
        "labels": labels.iter().map(|l| json!({ "name": l.name, "color": l.color.trim_start_matches('#') })).collect::<Vec<_>>(),
        "comments": issue.comment_count,
        "created_at": web::render::iso8601(issue.created),
    })
}

/// `opened`, `closed`, `reopened`, `edited`, `labeled`, `unlabeled`, `assigned`, ...
pub async fn issue(state: &AppState, repo: &db::Repo, number: i64, action: &str, actor: &db::User) {
    quietly("issues", async {
        let Some(issue) = db::issue(&state.db, repo.id, number).await? else {
            return Ok(());
        };
        let labels = db::labels_by_issue(&state.db, repo.id)
            .await?
            .remove(&issue.issue_id)
            .unwrap_or_default();
        let payload = json!({
            "action": action,
            "issue": issue_json(state, repo, &issue, &labels),
            "repository": repository(state, repo),
            "sender": sender(state, actor),
        });
        emit(state, repo, "issues", &payload).await
    })
    .await;
}

/// A new comment on an issue or pull request (`issue_comment`, like GitHub, for both).
pub async fn comment(state: &AppState, repo: &db::Repo, number: i64, body: &str, actor: &db::User) {
    quietly("issue_comment", async {
        let is_pull = db::number_kind(&state.db, repo.id, number).await? == Some(true);
        let base = format!("{}/{}/{}", state.site.http_url, repo.owner_name, repo.name);
        let issue = if is_pull {
            let Some(pull) = db::pull(&state.db, repo.id, number).await? else {
                return Ok(());
            };
            let mut value = pull_json(state, repo, &pull);
            value["pull_request"] = json!({ "html_url": format!("{base}/pull/{number}") });
            value
        } else {
            let Some(issue) = db::issue(&state.db, repo.id, number).await? else {
                return Ok(());
            };
            let labels = db::labels_by_issue(&state.db, repo.id)
                .await?
                .remove(&issue.issue_id)
                .unwrap_or_default();
            issue_json(state, repo, &issue, &labels)
        };
        let payload = json!({
            "action": "created",
            "issue": issue,
            "comment": { "body": body, "user": sender(state, actor) },
            "repository": repository(state, repo),
            "sender": sender(state, actor),
        });
        emit(state, repo, "issue_comment", &payload).await
    })
    .await;
}

/// Send a `ping` to one webhook, as GitHub does when it's created.
pub async fn ping(state: &AppState, repo: &db::Repo, hook_id: i64, actor: &db::User) -> Result<()> {
    let payload = json!({
        "zen": "Keep it logically awesome.",
        "hook_id": hook_id,
        "repository": repository(state, repo),
        "sender": sender(state, actor),
    });
    db::enqueue_delivery(&state.db, hook_id, "ping", &guid(), &payload.to_string()).await?;
    wakeup().notify_one();
    Ok(())
}

// ---- Delivery ----

pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes keys of any size");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Deliver queued events until the server stops.
pub fn spawn_worker(state: AppState) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("omaforge-Hookshot/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(%error, "can't start webhook deliveries");
                return;
            }
        };
        loop {
            match db::due_deliveries(&state.db, 20).await {
                Ok(batch) => {
                    let sends = batch
                        .into_iter()
                        .map(|(delivery, hook)| deliver(&state, &client, delivery, hook));
                    futures_util::future::join_all(sends).await;
                }
                Err(error) => tracing::warn!(%error, "can't read webhook queue"),
            }
            // Pushes are recorded by the hook process, so poll too.
            let _ = tokio::time::timeout(Duration::from_secs(1), wakeup().notified()).await;
        }
    });
}

async fn deliver(
    state: &AppState,
    client: &reqwest::Client,
    delivery: db::Delivery,
    hook: db::Webhook,
) {
    let started = std::time::Instant::now();
    let mut request = client
        .post(&hook.url)
        .header("content-type", "application/json")
        .header("x-github-event", &delivery.event)
        .header("x-github-delivery", &delivery.guid)
        .header("x-github-hook-id", hook.id.to_string())
        .body(delivery.payload.clone());
    if !hook.secret.is_empty() {
        request = request.header(
            "x-hub-signature-256",
            signature(&hook.secret, delivery.payload.as_bytes()),
        );
    }
    let result = request.send().await;
    let duration_ms = started.elapsed().as_millis() as i64;
    let (succeeded, status, body, error) = match result {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let body: String = body.chars().take(2000).collect();
            (
                status.is_success(),
                Some(i64::from(status.as_u16())),
                Some(body),
                None,
            )
        }
        Err(error) => (false, None, None, Some(error_chain(&error))),
    };
    let retry_in = (!succeeded)
        .then(|| RETRY_DELAYS.get(delivery.attempts as usize).copied())
        .flatten();
    let attempt = db::Attempt {
        response_status: status,
        response_body: body.as_deref(),
        error: error.as_deref(),
        duration_ms,
    };
    if let Err(e) = db::record_attempt(&state.db, delivery.id, &attempt, succeeded, retry_in).await
    {
        tracing::warn!(error = %e, "can't record webhook delivery");
    }
    tracing::debug!(hook = hook.id, event = %delivery.event, succeeded, ?status, "webhook delivery");
}

/// An error with its causes ("error sending request: connection refused"), since the
/// top-level message alone rarely says what went wrong.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_like_github() {
        // From GitHub's webhook docs: secret "It's a Secret to Everybody", payload "Hello, World!".
        assert_eq!(
            signature("It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }

    #[test]
    fn guids_look_like_uuids() {
        let id = guid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
    }
}
