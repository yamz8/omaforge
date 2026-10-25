//! Diffs as rows for the `diff_file.html` partial, shared by commit pages and pull
//! requests (which add review comments on lines).

use forge_git::diff::{FileDiff, FileStatus, LineKind};

/// Diffs with more lines than this have the remaining files collapsed.
const LINE_BUDGET: usize = 20_000;

pub struct InlineComment {
    pub id: i64,
    pub author: String,
    pub html: String,
    pub time_ago: String,
    /// Made on an earlier version of the pull request.
    pub outdated: bool,
}

/// What a comment form under a diff line submits.
pub struct LineCommentForm {
    pub action: String,
    pub csrf: String,
    pub path: String,
    pub side: &'static str,
    pub line: u32,
    pub commit: String,
    pub cancel_url: String,
}

pub struct DiffRow {
    /// `ctx`, `add`, `del`, `hunk` or `nonewline`.
    pub class: &'static str,
    pub old: String,
    pub new: String,
    pub text: String,
    /// The side (`old`/`new`) and line a comment on this row attaches to.
    pub anchor: Option<(&'static str, u32)>,
    /// Link that opens a comment form under this row, when commenting is possible.
    pub comment_url: Option<String>,
    pub comments: Vec<InlineComment>,
    /// A comment form to show under this row.
    pub form: Option<LineCommentForm>,
}

impl DiffRow {
    /// `id` for linking straight to this row.
    pub fn dom_id(&self) -> String {
        self.anchor
            .map(|(side, line)| format!("{side}{line}"))
            .unwrap_or_default()
    }
}

pub struct FileView {
    /// Stable id for links (`#diff-<anchor>-new12`), from the path, so links survive
    /// files being added to or removed from the diff.
    pub anchor: String,
    pub path: String,
    pub old_path: Option<String>,
    pub status: &'static str,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    /// Why the diff isn't shown inline, if it isn't.
    pub collapsed: Option<&'static str>,
    pub url: String,
    pub rows: Vec<DiffRow>,
    /// Comments on lines that aren't in this diff any more.
    pub outdated: Vec<InlineComment>,
}

/// Short hash of a path, for DOM ids.
pub fn path_hash(path: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(path.as_bytes())[..6])
}

/// Lock files and similar, whose diffs are noise; collapsed like GitHub does.
pub fn is_generated(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.lock"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "Gemfile.lock"
            | "composer.lock"
            | "poetry.lock"
            | "uv.lock"
            | "go.sum"
            | "flake.lock"
            | "mix.lock"
            | "Package.resolved"
    ) || name.ends_with(".min.js")
        || name.ends_with(".min.css")
}

/// Build the rows for each file. `file_url` gives the "View file" link.
pub fn file_views(diff: &[FileDiff], file_url: impl Fn(&FileDiff) -> String) -> Vec<FileView> {
    let mut budget = LINE_BUDGET;
    diff.iter()
        .map(|file| {
            let lines: usize = file.hunks.iter().map(|h| h.lines.len() + 1).sum();
            let collapsed = if is_generated(file.path()) {
                Some("Generated file not rendered.")
            } else if lines > budget {
                Some("Large diff not rendered.")
            } else {
                budget -= lines;
                None
            };
            let mut rows = Vec::new();
            if collapsed.is_none() {
                for hunk in &file.hunks {
                    rows.push(DiffRow {
                        class: "hunk",
                        old: String::new(),
                        new: String::new(),
                        text: hunk.header.clone(),
                        anchor: None,
                        comment_url: None,
                        comments: Vec::new(),
                        form: None,
                    });
                    for line in &hunk.lines {
                        let (class, anchor) = match line.kind {
                            LineKind::Context => ("ctx", line.new_line.map(|n| ("new", n))),
                            LineKind::Added => ("add", line.new_line.map(|n| ("new", n))),
                            LineKind::Deleted => ("del", line.old_line.map(|n| ("old", n))),
                            LineKind::NoNewline => ("nonewline", None),
                        };
                        let number = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                        rows.push(DiffRow {
                            class,
                            old: number(line.old_line),
                            new: number(line.new_line),
                            text: line.text.clone(),
                            anchor,
                            comment_url: None,
                            comments: Vec::new(),
                            form: None,
                        });
                    }
                }
            }
            FileView {
                anchor: path_hash(file.path()),
                path: file.path().to_owned(),
                old_path: (file.status == FileStatus::Renamed).then(|| file.old_path.clone()),
                status: match file.status {
                    FileStatus::Added => "added",
                    FileStatus::Deleted => "deleted",
                    FileStatus::Modified => "modified",
                    FileStatus::Renamed => "renamed",
                },
                additions: file.additions,
                deletions: file.deletions,
                binary: file.binary,
                collapsed,
                url: file_url(file),
                rows,
                outdated: Vec::new(),
            }
        })
        .collect()
}
