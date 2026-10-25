//! Git storage for omaforge: bare repositories on disk under `<data>/repos/<owner>/<name>.git`.

pub mod diff;
mod merge;
mod read;

pub use merge::{Identity, MergeMethod, Mergeability};

pub use read::{clean_path, Blob, Commit, EntryKind, ObjectKind, Ref, Repository, TreeEntry};

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};

/// Whether `name` is usable as a user or repo name. Names become path components, so this
/// is also what keeps request paths inside the repos directory.
pub fn is_valid_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && !name.starts_with('.')
        && !name.ends_with(".git")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Path of a repository's bare git directory.
pub fn repo_path(repos_root: &Path, owner: &str, name: &str) -> PathBuf {
    repos_root.join(owner).join(format!("{name}.git"))
}

/// Create a new bare repository with `main` as its default branch.
pub fn create_bare(repos_root: &Path, owner: &str, name: &str) -> Result<PathBuf> {
    if !is_valid_name(owner) || !is_valid_name(name) {
        bail!("invalid repository name {owner}/{name}");
    }
    let path = repo_path(repos_root, owner, name);
    if path.exists() {
        bail!("repository {owner}/{name} already exists");
    }
    std::fs::create_dir_all(&path)?;
    let status = Command::new("git")
        .args(["init", "--bare", "--quiet", "--initial-branch=main"])
        .arg(&path)
        .status()?;
    if !status.success() {
        bail!("git init failed for {owner}/{name}");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::is_valid_name;

    #[test]
    fn names() {
        for ok in ["omaforge", "my-repo", "a_b.c", "X1"] {
            assert!(is_valid_name(ok), "{ok}");
        }
        for bad in [
            "", ".", "..", ".hidden", "a/b", "a\\b", "repo.git", "sp ace", "ü",
        ] {
            assert!(!is_valid_name(bad), "{bad}");
        }
    }
}
