//! Comparing branches and merging them, entirely inside the bare repository (no working
//! tree): `merge-tree` computes merges, `commit-tree` and `replay` write commits, and the
//! branch only moves with a compare-and-swap `update-ref`.

use std::process::Stdio;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;

use crate::{
    diff::{self, FileDiff},
    read::{check_id, parse_commits, COMMIT_FORMAT},
    Commit, Repository,
};

/// Who a commit is attributed to.
#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    /// A merge commit with both branches as parents.
    Merge,
    /// One new commit on the base with all of the head's changes, authored by `author`.
    Squash,
    /// The head's commits replayed one by one onto the base.
    Rebase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mergeability {
    Clean,
    /// Files with conflicts.
    Conflicts(Vec<String>),
    /// Everything in head is already in base.
    UpToDate,
}

impl Repository {
    pub async fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        let out = self
            .try_run(["merge-base", check_id(a)?, check_id(b)?])
            .await?;
        Ok(out.map(|out| String::from_utf8_lossy(&out).trim().to_owned()))
    }

    /// Commits in `head` that aren't in `base`, oldest first.
    pub async fn compare_log(&self, base: &str, head: &str, limit: usize) -> Result<Vec<Commit>> {
        let range = format!("{}..{}", check_id(base)?, check_id(head)?);
        let out = self
            .run([
                "log",
                "--reverse",
                COMMIT_FORMAT,
                &format!("--max-count={limit}"),
                &range,
            ])
            .await?;
        Ok(parse_commits(&out))
    }

    /// The changes `head` makes since it diverged from `base` (`base...head`).
    pub async fn compare_diff(&self, base: &str, head: &str) -> Result<Vec<FileDiff>> {
        let range = format!("{}...{}", check_id(base)?, check_id(head)?);
        let out = self
            .run([
                "diff",
                "--patch",
                "--find-renames",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                &range,
            ])
            .await?;
        Ok(diff::parse(&String::from_utf8_lossy(&out)))
    }

    pub async fn mergeability(&self, base: &str, head: &str) -> Result<Mergeability> {
        let (base, head) = (check_id(base)?, check_id(head)?);
        if self.is_ancestor(head, base).await? {
            return Ok(Mergeability::UpToDate);
        }
        let output = self
            .command()
            .args([
                "merge-tree",
                "--write-tree",
                "--name-only",
                "--no-messages",
                base,
                head,
            ])
            .output()
            .await
            .context("running git merge-tree")?;
        match output.status.code() {
            Some(0) => Ok(Mergeability::Clean),
            // The first line is the (conflicted) tree; then the conflicting paths.
            Some(1) => Ok(Mergeability::Conflicts(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .skip(1)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )),
            _ => bail!(
                "git merge-tree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }

    pub async fn is_ancestor(&self, ancestor: &str, of: &str) -> Result<bool> {
        let status = self
            .command()
            .args([
                "merge-base",
                "--is-ancestor",
                check_id(ancestor)?,
                check_id(of)?,
            ])
            .status()
            .await?;
        Ok(status.success())
    }

    /// Merge `head` into `branch`, which must currently be at `base`. Returns the new tip of
    /// `branch`. Fails without changing anything if there are conflicts or if the branch
    /// moved in the meantime.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge(
        &self,
        branch: &str,
        base: &str,
        head: &str,
        method: MergeMethod,
        message: &str,
        author: &Identity,
        committer: &Identity,
    ) -> Result<String> {
        let (base, head) = (check_id(base)?, check_id(head)?);
        let new_tip = match method {
            MergeMethod::Merge | MergeMethod::Squash => {
                let output = self
                    .command()
                    .args(["merge-tree", "--write-tree", "--no-messages", base, head])
                    .output()
                    .await?;
                if !output.status.success() {
                    bail!("the branches have conflicts");
                }
                let tree = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let mut args = vec![
                    "commit-tree".to_owned(),
                    check_id(&tree)?.to_owned(),
                    "-p".into(),
                    base.into(),
                ];
                if method == MergeMethod::Merge {
                    args.extend(["-p".into(), head.into()]);
                }
                let author = if method == MergeMethod::Merge {
                    committer
                } else {
                    author
                };
                self.commit_tree(&args, message, author, committer).await?
            }
            MergeMethod::Rebase => {
                let merge_base = self
                    .merge_base(base, head)
                    .await?
                    .context("the branches share no history")?;
                let output = self
                    .command()
                    .env("GIT_COMMITTER_NAME", &committer.name)
                    .env("GIT_COMMITTER_EMAIL", &committer.email)
                    .args([
                        "replay".to_owned(),
                        "--ref-action=print".to_owned(),
                        format!("--advance={branch}"),
                        format!("{merge_base}..{head}"),
                    ])
                    .output()
                    .await?;
                if !output.status.success() {
                    bail!("the commits can't be rebased without conflicts");
                }
                // "update <ref> <new> <old>" for the replayed tip.
                let stdout = String::from_utf8_lossy(&output.stdout);
                let new = stdout
                    .lines()
                    .rev()
                    .find_map(|l| l.split_whitespace().nth(2))
                    .context("git replay produced no commits")?;
                check_id(new)?.to_owned()
            }
        };
        let status = self
            .command()
            .args([
                "update-ref",
                "-m",
                "merged by omaforge",
                &format!("refs/heads/{branch}"),
                &new_tip,
                base,
            ])
            .stderr(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            bail!("{branch} changed while merging; try again");
        }
        Ok(new_tip)
    }

    async fn commit_tree(
        &self,
        args: &[String],
        message: &str,
        author: &Identity,
        committer: &Identity,
    ) -> Result<String> {
        let mut child = self
            .command()
            .args(args)
            .env("GIT_AUTHOR_NAME", &author.name)
            .env("GIT_AUTHOR_EMAIL", &author.email)
            .env("GIT_COMMITTER_NAME", &committer.name)
            .env("GIT_COMMITTER_EMAIL", &committer.email)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().context("commit-tree stdin")?;
        stdin.write_all(message.as_bytes()).await?;
        drop(stdin);
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            bail!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(check_id(String::from_utf8_lossy(&output.stdout).trim())?.to_owned())
    }

    /// Delete a branch, but only if it still points at `expected`.
    pub async fn delete_branch(&self, branch: &str, expected: &str) -> Result<bool> {
        let status = self
            .command()
            .args([
                "update-ref",
                "-d",
                &format!("refs/heads/{branch}"),
                check_id(expected)?,
            ])
            .stderr(Stdio::null())
            .status()
            .await?;
        Ok(status.success())
    }
}
