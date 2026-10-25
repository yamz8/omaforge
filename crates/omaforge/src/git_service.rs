//! What the git transports (HTTP and SSH) have in common: the two git services and the
//! rules for who may use them on which repo.

use std::path::PathBuf;

use anyhow::Result;
use tokio::process::Command;

use crate::{
    auth::{self, Access},
    db::{self, User},
    AppState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    /// The `git` subcommand that implements this service.
    pub fn subcommand(self) -> &'static str {
        &self.name()["git-".len()..]
    }
}

/// Permission to run a git service on a repository.
#[derive(Debug, Clone)]
pub struct Grant {
    /// The repository's bare git directory.
    pub path: PathBuf,
    pub repo_id: i64,
    /// Who is pushing or fetching, if signed in.
    pub user_id: Option<i64>,
}

#[derive(Debug)]
pub enum Decision {
    Allowed(Grant),
    /// Anonymous and not allowed. Ask for credentials rather than revealing whether the
    /// repo exists.
    NeedsLogin,
    /// Signed in and may read, but this service needs write access.
    ReadOnly,
    /// Missing, or private and not readable by this user.
    NotFound,
}

/// Decide whether `user` may run `service` on the repo addressed by `owner`/`repo`
/// (`repo` may carry a `.git` suffix).
pub async fn authorize(
    state: &AppState,
    user: Option<&User>,
    owner: &str,
    repo: &str,
    service: Service,
) -> Result<Decision> {
    let name = repo.strip_suffix(".git").unwrap_or(repo);
    if !forge_git::is_valid_name(owner) || !forge_git::is_valid_name(name) {
        return Ok(Decision::NotFound);
    }
    let Some(repo) = db::repo_by_path(&state.db, owner, name).await? else {
        return Ok(if user.is_none() {
            Decision::NeedsLogin
        } else {
            Decision::NotFound
        });
    };

    let access = auth::access(&state.db, user, &repo).await?;
    let readable = access >= Access::Read;
    let allowed = match service {
        Service::UploadPack => readable,
        Service::ReceivePack => access >= Access::Write,
    };
    Ok(match (allowed, user) {
        (true, _) => Decision::Allowed(Grant {
            path: forge_git::repo_path(&state.repos, &repo.owner_name, &repo.name),
            repo_id: repo.id,
            user_id: user.map(|u| u.id),
        }),
        (false, None) => Decision::NeedsLogin,
        (false, Some(_)) if readable => Decision::ReadOnly,
        (false, Some(_)) => Decision::NotFound,
    })
}

/// The `git upload-pack` / `git receive-pack` process for a granted request, with
/// `extra` arguments before the repository path. Pushes run omaforge's hooks, which
/// enforce branch protection and record the push (see `hooks`).
pub fn command(
    state: &AppState,
    service: Service,
    grant: &Grant,
    extra: &[&str],
    protocol: Option<&str>,
) -> Command {
    let mut command = Command::new("git");
    if service == Service::ReceivePack {
        crate::hooks::configure(&mut command, state, grant);
    }
    command
        .arg(service.subcommand())
        .args(extra)
        .arg(&grant.path);
    if let Some(protocol) = protocol {
        command.env("GIT_PROTOCOL", protocol);
    }
    command
}
