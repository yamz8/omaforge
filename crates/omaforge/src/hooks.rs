//! Git hooks for pushes that go through omaforge.
//!
//! omaforge runs `git receive-pack` with `core.hooksPath` pointing at scripts in the data
//! directory, and tells them which repository and user the push is for. The scripts call
//! back into this binary (`omaforge hook <name>`):
//!
//! - `pre-receive` enforces branch protection. A refusal reaches the pusher as `remote:`
//!   lines, and git's quarantine means refused objects never enter the repository.
//! - `post-receive` records the push (webhooks).
//!
//! Changes omaforge makes itself (merging a pull request, deleting a branch) don't go
//! through `receive-pack`, so they record their own events.

use std::{
    io::{BufRead, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::{db, events, git_service::Grant, AppState};

const HOOKS: &[&str] = &["pre-receive", "post-receive"];

pub fn dir(data: &Path) -> PathBuf {
    data.join("hooks")
}

/// Write the hook scripts (on every start, so they always match this binary).
pub fn install(data: &Path) -> Result<()> {
    let dir = dir(data);
    std::fs::create_dir_all(&dir)?;
    for name in HOOKS {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\n\
             # Installed by omaforge; does nothing for pushes that don't go through omaforge.\n\
             [ -n \"$OMAFORGE_HOOK_BIN\" ] || exit 0\n\
             exec \"$OMAFORGE_HOOK_BIN\" --data \"$OMAFORGE_DATA\" hook {name}\n"
        );
        std::fs::write(&path, script).with_context(|| format!("writing {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn current_exe() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("omaforge")))
}

/// Make a `receive-pack` command run omaforge's hooks for this push.
pub fn configure(command: &mut Command, state: &AppState, grant: &Grant) {
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", dir(&state.data).display()))
        .env("OMAFORGE_HOOK_BIN", current_exe())
        .env("OMAFORGE_DATA", &state.data)
        .env("OMAFORGE_REPO_ID", grant.repo_id.to_string())
        .env(
            "OMAFORGE_PUSHER_ID",
            grant.user_id.map(|id| id.to_string()).unwrap_or_default(),
        )
        .env("OMAFORGE_PUBLIC_URL", &state.site.http_url);
}

/// One ref a push changes, as git gives them to hooks.
#[derive(Debug, Clone)]
pub struct RefUpdate {
    pub old: String,
    pub new: String,
    pub name: String,
}

impl RefUpdate {
    pub fn branch(&self) -> Option<&str> {
        self.name.strip_prefix("refs/heads/")
    }

    pub fn is_create(&self) -> bool {
        is_zero(&self.old)
    }

    pub fn is_delete(&self) -> bool {
        is_zero(&self.new)
    }
}

pub fn is_zero(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b == b'0')
}

fn read_updates() -> Vec<RefUpdate> {
    std::io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some(RefUpdate {
                old: parts.next()?.to_owned(),
                new: parts.next()?.to_owned(),
                name: parts.next()?.to_owned(),
            })
        })
        .collect()
}

/// `omaforge hook <name>`: returns the exit code for git.
pub async fn run(state: &AppState, name: &str) -> Result<i32> {
    let repo_id: i64 = match std::env::var("OMAFORGE_REPO_ID")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(id) => id,
        // Not a push through omaforge.
        None => return Ok(0),
    };
    let pusher_id: Option<i64> = std::env::var("OMAFORGE_PUSHER_ID")
        .ok()
        .and_then(|v| v.parse().ok());
    let updates = read_updates();
    match name {
        "pre-receive" => {
            let refusals = check_protection(state, repo_id, &updates).await?;
            if refusals.is_empty() {
                return Ok(0);
            }
            let mut stderr = std::io::stderr().lock();
            for refusal in &refusals {
                writeln!(stderr, "omaforge: {refusal}")?;
            }
            Ok(1)
        }
        "post-receive" => {
            if let Err(error) = events::pushed(state, repo_id, pusher_id, &updates).await {
                // The push already happened; don't make it look failed.
                eprintln!("omaforge: couldn't record the push: {error:#}");
            }
            Ok(0)
        }
        _ => Ok(0),
    }
}

/// Why each update would break a branch protection rule.
async fn check_protection(
    state: &AppState,
    repo_id: i64,
    updates: &[RefUpdate],
) -> Result<Vec<String>> {
    let rules = db::branch_rules(&state.db, repo_id).await?;
    let mut refusals = Vec::new();
    for update in updates {
        let Some(branch) = update.branch() else {
            continue;
        };
        let Some(rule) = db::rule_for_branch(&rules, branch) else {
            continue;
        };
        if update.is_delete() {
            if rule.block_deletion {
                refusals.push(format!("{branch} is protected and can't be deleted."));
            }
            continue;
        }
        if update.is_create() {
            continue;
        }
        if rule.block_force_push && !is_ancestor(&update.old, &update.new).await? {
            refusals.push(format!(
                "{branch} is protected: force pushes aren't allowed."
            ));
        } else if rule.require_pull_request {
            refusals.push(format!(
                "{branch} is protected: changes must go through a pull request."
            ));
        }
    }
    Ok(refusals)
}

/// Runs inside the hook, where git's environment (including the quarantine with the
/// pushed objects) points at the repository.
async fn is_ancestor(old: &str, new: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", old, new])
        .status()
        .await?;
    Ok(status.success())
}
