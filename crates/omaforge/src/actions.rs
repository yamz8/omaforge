//! Operations shared by the web UI and the API, so both behave exactly the same.

use anyhow::Result;
use forge_git::{Identity, MergeMethod, Repository};

use crate::{auth, db, events, hooks::RefUpdate, web, AppState};

/// Commits considered when looking for "fixes #N" in a merged pull request.
const COMMIT_LIMIT: usize = 250;

/// A pull request's branches resolved to commits.
pub struct Tips {
    pub base_sha: String,
    pub head_sha: String,
    pub base_exists: bool,
    pub head_exists: bool,
}

/// While a pull request is open, follow its branches to their current tips (and remember
/// them, so it survives its branch being deleted); once closed, use the stored tips.
pub async fn pull_tips(state: &AppState, git: &Repository, pull: &db::Pull) -> Result<Tips> {
    let branch = |name: &str| format!("refs/heads/{name}");
    let base = git.resolve_commit(&branch(&pull.base_branch)).await?;
    let head = git.resolve_commit(&branch(&pull.head_branch)).await?;
    let mut tips = Tips {
        base_exists: base.is_some(),
        head_exists: head.is_some(),
        base_sha: pull.base_sha.clone(),
        head_sha: pull.head_sha.clone(),
    };
    if pull.is_open() {
        if let Some(base) = base {
            tips.base_sha = base;
        }
        if let Some(head) = head {
            tips.head_sha = head;
        }
        if tips.base_sha != pull.base_sha || tips.head_sha != pull.head_sha {
            db::update_pull_shas(&state.db, pull.issue_id, &tips.base_sha, &tips.head_sha).await?;
        }
    }
    Ok(tips)
}

/// Who commits made through omaforge are attributed to: the user's email if they set
/// one, otherwise a no-reply address on this host.
pub fn identity(state: &AppState, username: &str, email: Option<String>) -> Identity {
    let host = web::url_host(&state.site.http_url);
    Identity {
        name: username.to_owned(),
        email: email.unwrap_or_else(|| format!("{username}@users.noreply.{host}")),
    }
}

pub fn parse_merge_method(name: &str) -> Option<MergeMethod> {
    match name {
        "merge" => Some(MergeMethod::Merge),
        "squash" => Some(MergeMethod::Squash),
        "rebase" => Some(MergeMethod::Rebase),
        _ => None,
    }
}

/// Merge a pull request as `user`, who must have write access (the caller checks).
/// Returns the new tip of the base branch, or why it couldn't be merged. Issues the pull
/// request says it fixes are closed.
pub async fn merge_pull(
    state: &AppState,
    repo: &db::Repo,
    git: &Repository,
    pull: &db::Pull,
    user: &db::User,
    method: MergeMethod,
) -> Result<Result<String, String>> {
    let tips = pull_tips(state, git, pull).await?;
    if !pull.is_open() || !tips.head_exists || !tips.base_exists {
        return Ok(Err("This pull request can't be merged.".into()));
    }
    if let Some(reason) = merge_blocker(state, repo, pull, &tips.head_sha).await? {
        return Ok(Err(reason));
    }
    let message = match method {
        MergeMethod::Squash if pull.body.is_empty() => {
            format!("{} (#{})\n", pull.title, pull.number)
        }
        MergeMethod::Squash => format!("{} (#{})\n\n{}\n", pull.title, pull.number, pull.body),
        _ => format!(
            "Merge pull request #{} from {}\n\n{}\n",
            pull.number, pull.head_branch, pull.title
        ),
    };
    let author = identity(
        state,
        &pull.author,
        db::user_email(&state.db, pull.author_id).await?,
    );
    let committer = identity(
        state,
        &user.username,
        db::user_email(&state.db, user.id).await?,
    );
    let merge_sha = match git
        .merge(
            &pull.base_branch,
            &tips.base_sha,
            &tips.head_sha,
            method,
            &message,
            &author,
            &committer,
        )
        .await
    {
        Ok(sha) => sha,
        Err(error) => return Ok(Err(format!("Couldn't merge: {error}."))),
    };
    let method_name = match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    };
    db::mark_merged(
        &state.db,
        &db::Merged {
            issue_id: pull.issue_id,
            merged_by: user.id,
            method: method_name,
            merge_sha: &merge_sha,
            base_sha: &tips.base_sha,
            head_sha: &tips.head_sha,
        },
    )
    .await?;
    tracing::info!(repo = %format!("{}/{}", repo.owner_name, repo.name), number = pull.number, user = %user.username, method = method_name, "merged pull request");
    // The merge moved the base branch without a push, so record it like one.
    let update = RefUpdate {
        old: tips.base_sha.clone(),
        new: merge_sha.clone(),
        name: format!("refs/heads/{}", pull.base_branch),
    };
    if let Err(error) = events::push(state, repo, user, &update).await {
        tracing::warn!(error = format!("{error:#}"), "couldn't record merge push");
    }
    events::pull_request(state, repo, pull.number, "closed", user).await;

    // "Fixes #12" in the pull request or its commits closes issue #12.
    let commits = git
        .compare_log(&tips.base_sha, &tips.head_sha, COMMIT_LIMIT)
        .await?;
    let mut text = format!("{}\n{}\n", pull.title, pull.body);
    for commit in &commits {
        text.push_str(&format!("{}\n{}\n", commit.subject, commit.body));
    }
    for closes in web::render::closing_references(&text) {
        if let Some(issue) = db::issue(&state.db, repo.id, closes).await? {
            db::close_issue_by(&state.db, issue.issue_id, user.id, pull.number).await?;
        }
    }
    Ok(Ok(merge_sha))
}

/// Why a pull request from `head` into `base` can't be opened, if it can't.
pub async fn check_new_pull(
    state: &AppState,
    repo: &db::Repo,
    git: &Repository,
    base: &str,
    head: &str,
) -> Result<Result<(String, String), &'static str>> {
    let branch = |name: &str| format!("refs/heads/{name}");
    let (Some(base_sha), Some(head_sha)) = (
        git.resolve_commit(&branch(base)).await?,
        git.resolve_commit(&branch(head)).await?,
    ) else {
        return Ok(Err("One of those branches doesn't exist."));
    };
    if base == head {
        return Ok(Err("Choose two different branches."));
    }
    if db::open_pull_for(&state.db, repo.id, base, head)
        .await?
        .is_some()
    {
        return Ok(Err(
            "There's already an open pull request for these branches.",
        ));
    }
    if git.compare_log(&base_sha, &head_sha, 1).await?.is_empty() {
        return Ok(Err("There's nothing to merge."));
    }
    Ok(Ok((base_sha, head_sha)))
}

/// `(approvals, changes requested)` that count for branch protection: the latest review
/// from each reviewer with write access (anyone can review, but only they decide).
pub async fn review_counts(state: &AppState, repo: &db::Repo, issue_id: i64) -> Result<(i64, i64)> {
    let mut latest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for review in db::reviews(&state.db, issue_id).await? {
        if review.state != "commented" {
            latest.insert(review.author, review.state);
        }
    }
    let mut counted = std::collections::HashMap::new();
    for (author, review_state) in latest {
        let Some(user) = db::user_by_username(&state.db, &author).await? else {
            continue;
        };
        if auth::access(&state.db, Some(&user), repo).await? >= auth::Access::Write {
            counted.insert(author, review_state);
        }
    }
    let latest = counted;
    let approvals = latest.values().filter(|s| *s == "approved").count() as i64;
    let changes = latest
        .values()
        .filter(|s| *s == "changes_requested")
        .count() as i64;
    Ok((approvals, changes))
}

/// What branch protection says stops this pull request from being merged, if anything.
pub async fn merge_blocker(
    state: &AppState,
    repo: &db::Repo,
    pull: &db::Pull,
    head_sha: &str,
) -> Result<Option<String>> {
    let rules = db::branch_rules(&state.db, repo.id).await?;
    let Some(rule) = db::rule_for_branch(&rules, &pull.base_branch) else {
        return Ok(None);
    };
    if rule.require_checks {
        let checks = crate::ci::Checks::load(state, repo.id, head_sha).await?;
        let base = &pull.base_branch;
        match checks.summary() {
            Some("success") => {}
            Some("failure") => {
                return Ok(Some(format!(
                    "{base} is protected: checks must pass, and some failed."
                )))
            }
            Some(_) => {
                return Ok(Some(format!(
                    "{base} is protected: waiting for checks to finish."
                )))
            }
            // A pull request can't get around required checks by removing the workflow.
            None => {
                return Ok(Some(format!(
                    "{base} is protected: checks must pass, but none have run on the latest commit."
                )))
            }
        }
    }
    if rule.required_approvals == 0 {
        return Ok(None);
    }
    let (approvals, changes) = review_counts(state, repo, pull.issue_id).await?;
    Ok(if changes > 0 {
        Some(format!(
            "{} is protected and changes were requested; they need to be approved first.",
            pull.base_branch
        ))
    } else if approvals < rule.required_approvals {
        let n = rule.required_approvals;
        Some(format!(
            "{} is protected: it needs {n} approving review{} from people with write access ({approvals} so far).",
            pull.base_branch,
            if n == 1 { "" } else { "s" }
        ))
    } else {
        None
    })
}
