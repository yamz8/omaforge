//! Branch protection rules.

use anyhow::{bail, Result};
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BranchRule {
    pub id: i64,
    pub pattern: String,
    pub require_pull_request: bool,
    pub required_approvals: i64,
    pub block_force_push: bool,
    pub block_deletion: bool,
}

pub async fn branch_rules(db: &SqlitePool, repo_id: i64) -> Result<Vec<BranchRule>> {
    let rules = sqlx::query_as(
        "SELECT id, pattern, require_pull_request, required_approvals, block_force_push, block_deletion
         FROM branch_rules WHERE repo_id = ? ORDER BY pattern",
    )
    .bind(repo_id)
    .fetch_all(db)
    .await?;
    Ok(rules)
}

pub struct NewBranchRule<'a> {
    pub pattern: &'a str,
    pub require_pull_request: bool,
    pub required_approvals: i64,
    pub block_force_push: bool,
    pub block_deletion: bool,
}

/// A pattern is a branch name where `*` may stand for any part of one path segment.
pub fn is_valid_pattern(pattern: &str) -> bool {
    (1..=200).contains(&pattern.len())
        && !pattern.starts_with('/')
        && !pattern.ends_with('/')
        && !pattern.contains("//")
        && !pattern.contains("..")
        && pattern
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'*'))
}

pub async fn add_branch_rule(
    db: &SqlitePool,
    repo_id: i64,
    rule: &NewBranchRule<'_>,
) -> Result<()> {
    if !is_valid_pattern(rule.pattern) {
        bail!("invalid branch pattern");
    }
    sqlx::query(
        "INSERT INTO branch_rules (repo_id, pattern, require_pull_request, required_approvals, block_force_push, block_deletion)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(repo_id)
    .bind(rule.pattern)
    .bind(rule.require_pull_request)
    .bind(rule.required_approvals.clamp(0, 10))
    .bind(rule.block_force_push)
    .bind(rule.block_deletion)
    .execute(db)
    .await
    .map_err(|error| match error {
        sqlx::Error::Database(e) if e.is_unique_violation() => {
            anyhow::anyhow!("there's already a rule for {:?}", rule.pattern)
        }
        error => error.into(),
    })?;
    Ok(())
}

pub async fn delete_branch_rule(db: &SqlitePool, repo_id: i64, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM branch_rules WHERE id = ? AND repo_id = ?")
        .bind(id)
        .bind(repo_id)
        .execute(db)
        .await?;
    Ok(())
}

/// The rule protecting `branch`: an exact match wins over a pattern.
pub fn rule_for_branch<'a>(rules: &'a [BranchRule], branch: &str) -> Option<&'a BranchRule> {
    rules.iter().find(|r| r.pattern == branch).or_else(|| {
        rules
            .iter()
            .find(|r| r.pattern.contains('*') && glob_match(&r.pattern, branch))
    })
}

/// `*` matches any run of characters except `/`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                go(&p[1..], n) || (n.first().is_some_and(|&c| c != b'/') && go(p, &n[1..]))
            }
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("main", "main"));
        assert!(glob_match("release/*", "release/1.0"));
        assert!(!glob_match("release/*", "release/1.0/hotfix"));
        assert!(glob_match("*", "main"));
        assert!(!glob_match("*", "feature/x"));
        assert!(glob_match("v*-stable", "v2-stable"));
        assert!(!glob_match("main", "main2"));
    }

    #[test]
    fn exact_rule_wins() {
        let rule = |id, pattern: &str| BranchRule {
            id,
            pattern: pattern.into(),
            require_pull_request: true,
            required_approvals: 0,
            block_force_push: true,
            block_deletion: true,
        };
        let rules = [rule(1, "*"), rule(2, "main")];
        assert_eq!(rule_for_branch(&rules, "main").map(|r| r.id), Some(2));
        assert_eq!(rule_for_branch(&rules, "dev").map(|r| r.id), Some(1));
        assert_eq!(rule_for_branch(&rules, "a/b").map(|r| r.id), None);
        assert!(
            is_valid_pattern("release/*") && !is_valid_pattern("../x") && !is_valid_pattern("a b")
        );
    }
}
