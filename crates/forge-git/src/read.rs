//! Reading repositories: refs, trees, blobs and history, by running `git` against the bare
//! repository. Everything that takes a revision from a user goes through
//! [`Repository::resolve_commit`] first; the other methods only accept commit ids.

use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::diff::{self, FileDiff};

pub struct Repository {
    git_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Ref {
    /// Short name, e.g. `main` or `v1.0`.
    pub name: String,
    /// The commit it points at (tags are peeled).
    pub commit: String,
    pub time: i64,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Tree,
    Blob,
    Symlink,
    /// A submodule (gitlink).
    Submodule,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub id: String,
    /// Size in bytes, for blobs.
    pub size: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Blob {
    pub size: u64,
    /// The contents, or `None` if the blob is larger than the requested limit.
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_time: i64,
    pub subject: String,
    pub body: String,
}

impl Commit {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(7)]
    }
}

/// What a path names inside a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Tree,
    Blob,
}

// Fields are separated by US and records by RS, which don't occur in names or messages.
pub(crate) const COMMIT_FORMAT: &str =
    "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ct%x1f%s%x1f%b%x1e";

impl Repository {
    pub fn open(git_dir: impl Into<PathBuf>) -> Self {
        Self {
            git_dir: git_dir.into(),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(["-c", "core.quotePath=false"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .kill_on_drop(true);
        command
    }

    /// Run git and return stdout, failing if git does.
    pub(crate) async fn run<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .command()
            .args(args)
            .output()
            .await
            .context("running git")?;
        if !output.status.success() {
            bail!(
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    /// Run git and return stdout, or `None` if git exits unsuccessfully (e.g. a missing object).
    pub(crate) async fn try_run<I, S>(&self, args: I) -> Result<Option<Vec<u8>>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self
            .command()
            .args(args)
            .output()
            .await
            .context("running git")?;
        Ok(output.status.success().then_some(output.stdout))
    }

    /// Whether the repository has no refs at all (nothing has been pushed yet).
    pub async fn is_empty(&self) -> Result<bool> {
        let out = self
            .run(["for-each-ref", "--count=1", "--format=x"])
            .await?;
        Ok(out.is_empty())
    }

    /// The branch to show by default: HEAD's branch if it exists, else the most recently
    /// updated branch.
    pub async fn default_branch(&self) -> Result<Option<String>> {
        if let Some(out) = self
            .try_run(["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await?
        {
            let head = String::from_utf8_lossy(&out).trim().to_owned();
            if self
                .try_run([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{head}"),
                ])
                .await?
                .is_some()
            {
                return Ok(Some(head));
            }
        }
        Ok(self
            .branches()
            .await?
            .into_iter()
            .next()
            .map(|branch| branch.name))
    }

    /// Branches, most recently updated first.
    pub async fn branches(&self) -> Result<Vec<Ref>> {
        self.refs("refs/heads").await
    }

    /// Tags, most recent first.
    pub async fn tags(&self) -> Result<Vec<Ref>> {
        self.refs("refs/tags").await
    }

    async fn refs(&self, prefix: &str) -> Result<Vec<Ref>> {
        let out = self
            .run([
                "for-each-ref",
                "--sort=-creatordate",
                "--format=%(refname:short)%1f%(objectname)%1f%(*objectname)%1f%(creatordate:unix)%1f%(contents:subject)%1e",
                prefix,
            ])
            .await?;
        Ok(records(&out)
            .filter_map(|record| {
                let [name, object, peeled, time, subject] = fields::<5>(record)?;
                Some(Ref {
                    name: name.to_owned(),
                    commit: if peeled.is_empty() { object } else { peeled }.to_owned(),
                    time: time.parse().unwrap_or(0),
                    subject: subject.to_owned(),
                })
            })
            .collect())
    }

    /// Names of all branches and tags, for splitting `rev/path` URLs.
    pub async fn ref_names(&self) -> Result<HashSet<String>> {
        let out = self
            .run([
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads",
                "refs/tags",
            ])
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Resolve a user-supplied revision (branch, tag or commit id) to a commit id.
    pub async fn resolve_commit(&self, rev: &str) -> Result<Option<String>> {
        if rev.is_empty() || rev.starts_with('-') {
            return Ok(None);
        }
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .try_run([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ])
            .await?;
        Ok(out.map(|out| String::from_utf8_lossy(&out).trim().to_owned()))
    }

    /// What `path` is in `commit`, or `None` if it doesn't exist. The empty path is the root.
    pub async fn object_kind(&self, commit: &str, path: &str) -> Result<Option<ObjectKind>> {
        let out = self
            .try_run(["cat-file", "-t", &object_spec(commit, path)?])
            .await?;
        Ok(
            match out
                .as_deref()
                .map(|out| String::from_utf8_lossy(out).trim().to_owned())
            {
                Some(kind) if kind == "tree" => Some(ObjectKind::Tree),
                Some(kind) if kind == "blob" => Some(ObjectKind::Blob),
                _ => None,
            },
        )
    }

    /// List a directory, directories first, then files, each alphabetically.
    pub async fn tree(&self, commit: &str, path: &str) -> Result<Vec<TreeEntry>> {
        let out = self
            .run(["ls-tree", "-z", "--long", &object_spec(commit, path)?])
            .await?;
        let mut entries: Vec<TreeEntry> = out
            .split(|&b| b == 0)
            .filter(|record| !record.is_empty())
            .filter_map(|record| {
                let record = String::from_utf8_lossy(record);
                // "<mode> <type> <id> <size>\t<name>"
                let (meta, name) = record.split_once('\t')?;
                let mut meta = meta.split_whitespace();
                let mode = meta.next()?;
                let _type = meta.next()?;
                let id = meta.next()?.to_owned();
                let size = meta.next()?.parse().ok();
                let kind = match mode {
                    "040000" => EntryKind::Tree,
                    "120000" => EntryKind::Symlink,
                    "160000" => EntryKind::Submodule,
                    _ => EntryKind::Blob,
                };
                Some(TreeEntry {
                    name: name.to_owned(),
                    kind,
                    id,
                    size,
                })
            })
            .collect();
        entries.sort_by(|a, b| {
            (a.kind != EntryKind::Tree)
                .cmp(&(b.kind != EntryKind::Tree))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// Read a file, returning its contents if it is at most `limit` bytes.
    pub async fn blob(&self, commit: &str, path: &str, limit: u64) -> Result<Blob> {
        let spec = object_spec(commit, path)?;
        let size = String::from_utf8_lossy(&self.run(["cat-file", "-s", &spec]).await?)
            .trim()
            .parse()
            .context("parsing blob size")?;
        let data = if size <= limit {
            Some(self.run(["cat-file", "blob", &spec]).await?)
        } else {
            None
        };
        Ok(Blob { size, data })
    }

    /// History of `commit`, optionally limited to commits touching `path`.
    pub async fn log(
        &self,
        commit: &str,
        path: &str,
        skip: usize,
        limit: usize,
    ) -> Result<Vec<Commit>> {
        let commit = check_id(commit)?;
        let mut args = vec![
            "log".to_owned(),
            COMMIT_FORMAT.to_owned(),
            format!("--skip={skip}"),
            format!("--max-count={limit}"),
            commit.to_owned(),
        ];
        if !path.is_empty() {
            args.extend(["--".to_owned(), clean_path(path)?]);
        }
        let out = self.run(&args).await?;
        Ok(parse_commits(&out))
    }

    /// Number of commits reachable from `commit`.
    pub async fn commit_count(&self, commit: &str) -> Result<usize> {
        let out = self.run(["rev-list", "--count", check_id(commit)?]).await?;
        Ok(String::from_utf8_lossy(&out).trim().parse()?)
    }

    pub async fn commit(&self, id: &str) -> Result<Option<Commit>> {
        let id = check_id(id)?;
        let out = self
            .try_run(["show", "--no-patch", COMMIT_FORMAT, id])
            .await?;
        Ok(out.and_then(|out| records(&out).next().and_then(parse_commit)))
    }

    /// The changes a commit introduced, relative to its first parent.
    pub async fn diff(&self, id: &str) -> Result<Vec<FileDiff>> {
        let id = check_id(id)?;
        let out = self
            .run([
                "diff-tree",
                "--patch",
                "--find-renames",
                "--root",
                "--first-parent",
                "-m",
                "--no-commit-id",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                id,
            ])
            .await?;
        Ok(diff::parse(&String::from_utf8_lossy(&out)))
    }
}

/// Normalize a path from a URL: no empty, `.` or `..` components.
pub fn clean_path(path: &str) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            bail!("invalid path {path:?}");
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

pub(crate) fn check_id(id: &str) -> Result<&str> {
    if (4..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(id)
    } else {
        bail!("not a commit id: {id:?}")
    }
}

/// `<commit>:<path>`, the git syntax for an object inside a commit's tree.
fn object_spec(commit: &str, path: &str) -> Result<String> {
    Ok(format!("{}:{}", check_id(commit)?, clean_path(path)?))
}

fn records(out: &[u8]) -> impl Iterator<Item = &str> {
    std::str::from_utf8(out)
        .unwrap_or_default()
        .split('\x1e')
        .map(|record| record.trim_start_matches('\n'))
        .filter(|record| !record.is_empty())
}

fn fields<const N: usize>(record: &str) -> Option<[&str; N]> {
    let mut parts = record.splitn(N, '\x1f');
    let mut fields = [""; N];
    for field in &mut fields {
        *field = parts.next()?;
    }
    Some(fields)
}

pub(crate) fn parse_commits(out: &[u8]) -> Vec<Commit> {
    records(out).filter_map(parse_commit).collect()
}

fn parse_commit(record: &str) -> Option<Commit> {
    let [id, parents, author_name, author_email, author_time, committer_name, committer_time, subject, body] =
        fields::<9>(record)?;
    Some(Commit {
        id: id.to_owned(),
        parents: parents.split_whitespace().map(str::to_owned).collect(),
        author_name: author_name.to_owned(),
        author_email: author_email.to_owned(),
        author_time: author_time.parse().unwrap_or(0),
        committer_name: committer_name.to_owned(),
        committer_time: committer_time.parse().unwrap_or(0),
        subject: subject.to_owned(),
        body: body.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_paths() {
        assert_eq!(clean_path("").unwrap(), "");
        assert_eq!(clean_path("/src//main.rs/").unwrap(), "src/main.rs");
        assert!(clean_path("src/../etc").is_err());
        assert!(clean_path("./x").is_err());
    }

    #[test]
    fn checks_ids() {
        assert!(check_id("0123abcd").is_ok());
        assert!(check_id("main").is_err());
        assert!(check_id("--output=x").is_err());
    }
}
