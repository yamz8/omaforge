//! `omaforge import local`: copy existing repositories from disk into omaforge.
//!
//! Each source is fetched *into* omaforge's bare repository, so nothing runs in the source
//! repository (no hooks). Local branches and tags are imported, plus branches that only
//! exist on `origin`, so a clone of a remote keeps everything it knew about.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};

use crate::{db, AppState};

pub struct Options {
    pub owner: String,
    pub public: bool,
    /// Fetch into repositories that already exist instead of skipping them.
    pub update: bool,
    pub dry_run: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Imported { branches: usize, tags: usize },
    Updated { branches: usize, tags: usize },
    Skipped(String),
}

pub async fn local(state: &AppState, paths: &[PathBuf], options: &Options) -> Result<()> {
    let owner = db::user_by_username(&state.db, &options.owner)
        .await?
        .with_context(|| format!("no such user {:?}", options.owner))?;
    let (mut imported, mut skipped, mut failed) = (0, 0, 0);
    for path in paths {
        let label = path.display();
        match import_one(state, &owner, path, options).await {
            Ok((name, Outcome::Imported { branches, tags })) => {
                imported += 1;
                let verb = if options.dry_run {
                    "would import"
                } else {
                    "imported"
                };
                println!(
                    "{verb} {label} -> {}/{name} ({})",
                    owner.username,
                    counts(branches, tags)
                );
            }
            Ok((name, Outcome::Updated { branches, tags })) => {
                imported += 1;
                let verb = if options.dry_run {
                    "would update"
                } else {
                    "updated"
                };
                println!(
                    "{verb} {label} -> {}/{name} ({})",
                    owner.username,
                    counts(branches, tags)
                );
            }
            Ok((_, Outcome::Skipped(reason))) => {
                skipped += 1;
                println!("skipped {label}: {reason}");
            }
            Err(error) => {
                failed += 1;
                println!("failed {label}: {error:#}");
            }
        }
    }
    println!("{imported} imported, {skipped} skipped, {failed} failed");
    if failed > 0 {
        bail!("{failed} imports failed");
    }
    Ok(())
}

fn counts(branches: usize, tags: usize) -> String {
    let branches = if branches == 1 {
        "1 branch".to_owned()
    } else {
        format!("{branches} branches")
    };
    let tags = if tags == 1 {
        "1 tag".to_owned()
    } else {
        format!("{tags} tags")
    };
    format!("{branches}, {tags}")
}

async fn import_one(
    state: &AppState,
    owner: &db::User,
    path: &Path,
    options: &Options,
) -> Result<(String, Outcome)> {
    let Some(Source {
        git_dir: source,
        bare,
    }) = source(path)
    else {
        return Ok((
            String::new(),
            Outcome::Skipped("not a git repository".into()),
        ));
    };
    let Some(name) = repo_name(path) else {
        return Ok((
            String::new(),
            Outcome::Skipped("can't make a repository name from this folder".into()),
        ));
    };
    let refs = refs_to_import(&source)?;
    if refs.is_empty() {
        return Ok((name, Outcome::Skipped("no commits".into())));
    }
    for warning in leftovers(path, &source, bare) {
        println!("  note: {} {warning} (not imported)", path.display());
    }

    let exists = db::repo_by_path(&state.db, &owner.username, &name)
        .await?
        .is_some();
    if exists && !options.update {
        let reason = format!("{}/{name} already exists (use --update)", owner.username);
        return Ok((name, Outcome::Skipped(reason)));
    }
    let branches = refs
        .iter()
        .filter(|r| r.dest.starts_with("refs/heads/"))
        .count();
    let tags = refs.len() - branches;
    let outcome = if exists {
        Outcome::Updated { branches, tags }
    } else {
        Outcome::Imported { branches, tags }
    };
    if options.dry_run {
        return Ok((name, outcome));
    }

    let dest = forge_git::repo_path(&state.repos, &owner.username, &name);
    if exists {
        fetch(&dest, &source, &refs)?;
    } else {
        let description = description(&source);
        db::create_repo(
            &state.db,
            &state.repos,
            owner,
            &name,
            &description,
            !options.public,
        )
        .await?;
        // Don't leave an empty repository behind if the copy fails.
        if let Err(error) =
            fetch(&dest, &source, &refs).and_then(|()| set_default_branch(&dest, &source, &refs))
        {
            db::delete_repo(&state.db, owner.id, &name).await?;
            std::fs::remove_dir_all(&dest).ok();
            return Err(error);
        }
    }
    Ok((name, outcome))
}

struct Source {
    git_dir: PathBuf,
    bare: bool,
}

fn rev_parse(path: &Path, args: &[&str]) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

/// The repository at `path`: a bare repository, or a working tree whose top level is
/// `path` (not just some folder inside another repository).
fn source(path: &Path) -> Option<Source> {
    let path = path.canonicalize().ok()?;
    let git_dir = rev_parse(&path, &["--absolute-git-dir"])?
        .canonicalize()
        .ok()?;
    if rev_parse(&path, &["--is-bare-repository"])?.as_os_str() == "true" {
        return (git_dir == path).then_some(Source {
            git_dir,
            bare: true,
        });
    }
    let top = rev_parse(&path, &["--show-toplevel"])?
        .canonicalize()
        .ok()?;
    (top == path).then_some(Source {
        git_dir,
        bare: false,
    })
}

/// `my project.git` -> `my-project`. `None` if nothing usable is left.
fn repo_name(path: &Path) -> Option<String> {
    let base = path
        .canonicalize()
        .ok()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let mut name: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    while let Some(stripped) = name.strip_suffix(".git") {
        name = stripped.to_owned();
    }
    let name = name.trim_matches(|c| c == '-' || c == '.').to_owned();
    forge_git::is_valid_name(&name).then_some(name)
}

#[derive(Debug, PartialEq, Eq)]
struct RefToImport {
    source: String,
    dest: String,
}

/// Local branches and tags, plus `origin` branches with no local branch of the same name.
fn refs_to_import(git_dir: &Path) -> Result<Vec<RefToImport>> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/tags",
            "refs/remotes/origin",
        ])
        .output()
        .context("listing refs")?;
    if !output.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(plan_refs(&String::from_utf8_lossy(&output.stdout)))
}

fn plan_refs(listing: &str) -> Vec<RefToImport> {
    let names: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut refs = Vec::new();
    for name in &names {
        if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
            refs.push(RefToImport {
                source: (*name).to_owned(),
                dest: (*name).to_owned(),
            });
        } else if let Some(branch) = name.strip_prefix("refs/remotes/origin/") {
            let local = format!("refs/heads/{branch}");
            if branch != "HEAD" && !names.contains(&local.as_str()) {
                refs.push(RefToImport {
                    source: (*name).to_owned(),
                    dest: local,
                });
            }
        }
    }
    refs
}

/// Things in the working copy that git history doesn't carry.
fn leftovers(path: &Path, git_dir: &Path, bare: bool) -> Vec<String> {
    let mut notes = Vec::new();
    if !bare {
        let changes = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["status", "--porcelain"])
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0);
        if changes > 0 {
            notes.push(format!(
                "has {changes} uncommitted or untracked file{}",
                if changes == 1 { "" } else { "s" }
            ));
        }
    }
    let stashes = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["stash", "list"])
        .stderr(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0);
    if stashes > 0 {
        notes.push(format!(
            "has {stashes} stash entr{}",
            if stashes == 1 { "y" } else { "ies" }
        ));
    }
    notes
}

/// The repository's own description, unless it's git's placeholder.
fn description(git_dir: &Path) -> String {
    std::fs::read_to_string(git_dir.join("description"))
        .map(|d| d.trim().to_owned())
        .ok()
        .filter(|d| !d.is_empty() && !d.starts_with("Unnamed repository"))
        .unwrap_or_default()
}

/// Fetch `refs` from `source` into the bare repository `dest`. Without `+` in the
/// refspecs, existing branches only ever fast-forward.
fn fetch(dest: &Path, source: &Path, refs: &[RefToImport]) -> Result<()> {
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(dest)
        .args([
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            "--no-progress",
            "--stdin",
        ])
        .arg(source)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git fetch")?;
    {
        let mut stdin = child.stdin.take().context("git fetch stdin")?;
        for r in refs {
            writeln!(stdin, "{}:{}", r.source, r.dest)?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // " ! [rejected]        wip        -> wip  (non-fast-forward)"
        let rejected: Vec<&str> = stderr
            .lines()
            .filter(|line| line.contains("[rejected]"))
            .filter_map(|line| line.split("->").nth(1)?.split_whitespace().next())
            .collect();
        if rejected.is_empty() {
            bail!("git fetch: {}", stderr.trim());
        }
        bail!(
            "history was rewritten, so these were left unchanged: {} (other refs were updated)",
            rejected.join(", ")
        );
    }
    Ok(())
}

/// Point HEAD at the source's current branch, or `main`/`master`, or any branch.
fn set_default_branch(dest: &Path, source: &Path, refs: &[RefToImport]) -> Result<()> {
    let source_head = Command::new("git")
        .arg("--git-dir")
        .arg(source)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let branches: Vec<&str> = refs
        .iter()
        .map(|r| r.dest.as_str())
        .filter(|d| d.starts_with("refs/heads/"))
        .collect();
    let head = source_head
        .filter(|h| branches.contains(&h.as_str()))
        .or_else(|| {
            ["refs/heads/main", "refs/heads/master"]
                .into_iter()
                .find(|b| branches.contains(b))
                .map(str::to_owned)
        })
        .or_else(|| branches.first().map(|b| (*b).to_owned()));
    if let Some(head) = head {
        let status = Command::new("git")
            .arg("--git-dir")
            .arg(dest)
            .args(["symbolic-ref", "HEAD", &head])
            .status()?;
        if !status.success() {
            bail!("setting HEAD to {head}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_refs() {
        let refs = plan_refs(
            "refs/heads/main\nrefs/heads/wip\nrefs/remotes/origin/HEAD\nrefs/remotes/origin/main\n\
             refs/remotes/origin/feature/x\nrefs/tags/v1\n",
        );
        let pairs: Vec<(&str, &str)> = refs
            .iter()
            .map(|r| (r.source.as_str(), r.dest.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("refs/heads/main", "refs/heads/main"),
                ("refs/heads/wip", "refs/heads/wip"),
                // origin/main is skipped: the local main wins.
                ("refs/remotes/origin/feature/x", "refs/heads/feature/x"),
                ("refs/tags/v1", "refs/tags/v1"),
            ]
        );
    }
}
