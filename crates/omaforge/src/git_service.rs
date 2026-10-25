//! What the git transports (HTTP and SSH) have in common: the two git services and the
//! rules for who may use them on which repo.

use std::path::PathBuf;

use anyhow::Result;

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

#[derive(Debug)]
pub enum Decision {
    /// Allowed; the repo's bare git directory.
    Allowed(PathBuf),
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
        (true, _) => Decision::Allowed(forge_git::repo_path(
            &state.repos,
            &repo.owner_name,
            &repo.name,
        )),
        (false, None) => Decision::NeedsLogin,
        (false, Some(_)) if readable => Decision::ReadOnly,
        (false, Some(_)) => Decision::NotFound,
    })
}
