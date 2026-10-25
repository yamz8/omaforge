//! Merging inside bare repositories, against real git.

use std::{path::Path, process::Command};

use forge_git::{Identity, MergeMethod, Mergeability, Repository};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Never read or write the developer's own git config.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A bare repository with `main`, a `feature` branch that merges cleanly, and a
/// `conflict` branch that doesn't.
fn fixture(root: &Path) -> std::path::PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("f"), "1\n2\n3\n").unwrap();
    git(&work, &["add", "f"]);
    git(&work, &["commit", "-qm", "base"]);
    git(&work, &["checkout", "-qb", "feature"]);
    std::fs::write(work.join("g"), "feature\n").unwrap();
    git(&work, &["add", "g"]);
    git(&work, &["commit", "-qm", "add g"]);
    std::fs::write(work.join("g"), "feature 2\n").unwrap();
    git(&work, &["commit", "-qam", "change g"]);
    git(&work, &["checkout", "-qb", "conflict", "main"]);
    std::fs::write(work.join("f"), "1\nTWO\n3\n").unwrap();
    git(&work, &["commit", "-qam", "conflict"]);
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("f"), "1\ndeux\n3\n").unwrap();
    git(&work, &["commit", "-qam", "main moves on"]);
    let bare = root.join("repo.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("forge-git-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn ids() -> (Identity, Identity) {
    (
        Identity {
            name: "Pat Author".into(),
            email: "pat@example.com".into(),
        },
        Identity {
            name: "Mo Merger".into(),
            email: "mo@example.com".into(),
        },
    )
}

#[tokio::test]
async fn mergeability() {
    let root = temp("mergeability");
    let bare = fixture(&root);
    let repo = Repository::open(&bare);
    let main = git(&bare, &["rev-parse", "main"]);
    let feature = git(&bare, &["rev-parse", "feature"]);
    let conflict = git(&bare, &["rev-parse", "conflict"]);
    assert_eq!(
        repo.mergeability(&main, &feature).await.unwrap(),
        Mergeability::Clean
    );
    assert_eq!(
        repo.mergeability(&main, &conflict).await.unwrap(),
        Mergeability::Conflicts(vec!["f".into()])
    );
    let base = git(&bare, &["rev-parse", "main~1"]);
    assert_eq!(
        repo.mergeability(&main, &base).await.unwrap(),
        Mergeability::UpToDate
    );

    let log = repo.compare_log(&main, &feature, 50).await.unwrap();
    assert_eq!(
        log.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(),
        ["add g", "change g"]
    );
    let diff = repo.compare_diff(&main, &feature).await.unwrap();
    assert_eq!(
        diff.len(),
        1,
        "only the feature's own change, not main's: {diff:?}"
    );
    assert_eq!(diff[0].path(), "g");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn merge_methods() {
    let (author, committer) = ids();
    for method in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
        let root = temp(&format!("{method:?}"));
        let bare = fixture(&root);
        let repo = Repository::open(&bare);
        let main = git(&bare, &["rev-parse", "main"]);
        let feature = git(&bare, &["rev-parse", "feature"]);
        let tip = repo
            .merge(
                "main",
                &main,
                &feature,
                method,
                "Merge pull request #1\n",
                &author,
                &committer,
            )
            .await
            .unwrap();
        assert_eq!(
            git(&bare, &["rev-parse", "main"]),
            tip,
            "{method:?} moved main"
        );
        assert_eq!(
            git(&bare, &["show", "main:g"]),
            "feature 2",
            "{method:?} has the change"
        );
        assert_eq!(
            git(&bare, &["show", "main:f"]),
            "1\ndeux\n3",
            "{method:?} kept main's change"
        );
        let parents = git(&bare, &["log", "-1", "--format=%P", "main"])
            .split_whitespace()
            .count();
        let (who, by) = (
            git(&bare, &["log", "-1", "--format=%an", "main"]),
            git(&bare, &["log", "-1", "--format=%cn", "main"]),
        );
        match method {
            MergeMethod::Merge => {
                assert_eq!(parents, 2);
                assert_eq!((who.as_str(), by.as_str()), ("Mo Merger", "Mo Merger"));
            }
            MergeMethod::Squash => {
                assert_eq!(parents, 1);
                assert_eq!(
                    git(&bare, &["log", "-1", "--format=%s", "main"]),
                    "Merge pull request #1"
                );
                assert_eq!((who.as_str(), by.as_str()), ("Pat Author", "Mo Merger"));
            }
            MergeMethod::Rebase => {
                assert_eq!(parents, 1);
                assert_eq!(
                    git(&bare, &["log", "-2", "--format=%s", "main"]),
                    "change g\nadd g"
                );
                // The original author is kept; the merger becomes the committer.
                assert_eq!((who.as_str(), by.as_str()), ("Ada", "Mo Merger"));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn refuses_conflicts_and_moved_branches() {
    let (author, committer) = ids();
    let root = temp("refuses");
    let bare = fixture(&root);
    let repo = Repository::open(&bare);
    let main = git(&bare, &["rev-parse", "main"]);
    let conflict = git(&bare, &["rev-parse", "conflict"]);
    let feature = git(&bare, &["rev-parse", "feature"]);
    for method in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
        assert!(repo
            .merge("main", &main, &conflict, method, "m", &author, &committer)
            .await
            .is_err());
        assert_eq!(
            git(&bare, &["rev-parse", "main"]),
            main,
            "{method:?} left main alone"
        );
    }
    // Someone pushed to main after we looked at it: the merge must not overwrite it.
    let stale = git(&bare, &["rev-parse", "main~1"]);
    let error = repo
        .merge(
            "main",
            &stale,
            &feature,
            MergeMethod::Merge,
            "m",
            &author,
            &committer,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("changed while merging"),
        "{error}"
    );
    assert_eq!(git(&bare, &["rev-parse", "main"]), main);

    assert!(
        !repo.delete_branch("feature", &main).await.unwrap(),
        "wrong expected id"
    );
    assert!(repo.delete_branch("feature", &feature).await.unwrap());
    std::fs::remove_dir_all(root).unwrap();
}
