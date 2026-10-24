//! Parse `git diff` unified patches into files, hunks and numbered lines.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    pub additions: usize,
    pub deletions: usize,
}

impl FileDiff {
    /// The path to show for this file: the new path, unless it was deleted.
    pub fn path(&self) -> &str {
        if self.status == FileStatus::Deleted {
            &self.old_path
        } else {
            &self.new_path
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ context` line.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Deleted,
    /// `\ No newline at end of file`
    NoNewline,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// The line without its leading `+`, `-` or space.
    pub text: String,
}

pub fn parse(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let (mut old_line, mut new_line) = (0u32, 0u32);
    // Lines still expected in the current hunk, from its `@@ -a,b +c,d @@` header. Counting
    // (rather than looking at prefixes) keeps a deleted "-- comment" line from being taken
    // for a `--- a/file` header.
    let (mut old_left, mut new_left) = (0u32, 0u32);

    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let (old_path, new_path) = split_header_paths(header);
            files.push(FileDiff {
                old_path,
                new_path,
                status: FileStatus::Modified,
                binary: false,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };

        let in_hunk = old_left > 0 || new_left > 0;
        if let Some(hunk) = file
            .hunks
            .last_mut()
            .filter(|_| in_hunk || line.starts_with('\\'))
        {
            let (kind, text) = match line.chars().next() {
                Some('+') => (LineKind::Added, &line[1..]),
                Some('-') => (LineKind::Deleted, &line[1..]),
                Some('\\') => (LineKind::NoNewline, line),
                Some(_) => (LineKind::Context, &line[1..]),
                None => (LineKind::Context, ""),
            };
            let (old, new) = match kind {
                LineKind::Context => {
                    old_line += 1;
                    new_line += 1;
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    (Some(old_line), Some(new_line))
                }
                LineKind::Added => {
                    new_line += 1;
                    new_left = new_left.saturating_sub(1);
                    file.additions += 1;
                    (None, Some(new_line))
                }
                LineKind::Deleted => {
                    old_line += 1;
                    old_left = old_left.saturating_sub(1);
                    file.deletions += 1;
                    (Some(old_line), None)
                }
                LineKind::NoNewline => (None, None),
            };
            hunk.lines.push(DiffLine {
                kind,
                old_line: old,
                new_line: new,
                text: text.to_owned(),
            });
        } else if line.starts_with("@@ ") {
            (old_line, old_left, new_line, new_left) = hunk_ranges(line);
            file.hunks.push(Hunk {
                header: line.to_owned(),
                lines: Vec::new(),
            });
        } else if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(path) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = unquote(path);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.new_path = unquote(path);
        } else if let Some(path) = line.strip_prefix("--- ") {
            if let Some(path) = strip_side(path, "a/") {
                file.old_path = path;
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(path) = strip_side(path, "b/") {
                file.new_path = path;
            }
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }
    files
}

/// From `@@ -a,b +c,d @@`: (a - 1, b, c - 1, d). Starts are one less because the line
/// counters are incremented before each line; a missing count means 1.
fn hunk_ranges(header: &str) -> (u32, u32, u32, u32) {
    let range = |part: Option<&str>| {
        let mut numbers = part
            .unwrap_or_default()
            .get(1..)
            .unwrap_or_default()
            .split(',');
        let start = numbers
            .next()
            .and_then(|n| n.parse::<u32>().ok())
            .unwrap_or(0);
        let count = numbers
            .next()
            .map_or(Some(1), |n| n.parse().ok())
            .unwrap_or(0);
        (start.saturating_sub(1), count)
    };
    let mut parts = header.split_whitespace().skip(1);
    let (old_start, old_count) = range(parts.next());
    let (new_start, new_count) = range(parts.next());
    (old_start, old_count, new_start, new_count)
}

/// `--- a/path` / `+++ b/path`, or `/dev/null`.
fn strip_side(path: &str, prefix: &str) -> Option<String> {
    let path = unquote(path.trim_end_matches('\t'));
    path.strip_prefix(prefix).map(str::to_owned)
}

/// Split `a/old b/new` from a `diff --git` line. Ambiguous when names contain " b/", in
/// which case the `---`/`+++`/rename lines that follow correct it (except for binary or
/// mode-only changes of such files).
fn split_header_paths(header: &str) -> (String, String) {
    if let Some(rest) = header.strip_prefix('"') {
        // Quoted names: `"a/x y" "b/x y"`.
        if let Some(end) = rest.find("\" ").map(|i| i + 1) {
            let old = unquote(&header[..=end]);
            let new = unquote(header[end + 2..].trim());
            return (
                old.strip_prefix("a/").unwrap_or(&old).to_owned(),
                new.strip_prefix("b/").unwrap_or(&new).to_owned(),
            );
        }
    }
    // Unrenamed files have identical halves, so split in the middle when that works.
    let middle = header.len() / 2;
    if header.is_char_boundary(middle)
        && header[middle..].starts_with(" b/")
        && header[2..middle] == header[middle + 3..]
    {
        let path = header[2..middle].to_owned();
        return (path.clone(), path);
    }
    match header.split_once(" b/") {
        Some((old, new)) => (old.trim_start_matches("a/").to_owned(), new.to_owned()),
        None => (header.to_owned(), header.to_owned()),
    }
}

/// Undo git's C-style quoting of unusual file names (`"a\tb"`, octal escapes for bytes).
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(d @ b'0'..=b'7') => {
                let mut value = u32::from(d - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(&d @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(d - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,4 @@ fn main() {
 one
-two
+deux
+trois
 three
--- sql comment
diff --git a/new file.txt b/new file.txt
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/new file.txt
@@ -0,0 +1 @@
+hello
\\ No newline at end of file
diff --git a/old.txt b/renamed.txt
similarity index 100%
rename from old.txt
rename to renamed.txt
diff --git a/logo.png b/logo.png
new file mode 100644
index 0000000..4444444
Binary files /dev/null and b/logo.png differ
diff --git \"a/tab\\there\" \"b/tab\\there\"
deleted file mode 100644
index 5555555..0000000
--- \"a/tab\\there\"
+++ /dev/null
@@ -1 +0,0 @@
-gone
";

    #[test]
    fn parses_patch() {
        let files = parse(PATCH);
        assert_eq!(files.len(), 5);

        let main = &files[0];
        assert_eq!(
            (main.path(), main.status),
            ("src/main.rs", FileStatus::Modified)
        );
        assert_eq!((main.additions, main.deletions), (2, 2));
        let lines = &main.hunks[0].lines;
        assert_eq!(lines.len(), 6);
        assert_eq!(
            (lines[5].kind, lines[5].text.as_str()),
            (LineKind::Deleted, "-- sql comment")
        );
        assert_eq!((lines[0].old_line, lines[0].new_line), (Some(1), Some(1)));
        assert_eq!(
            (lines[1].kind, lines[1].old_line, lines[1].text.as_str()),
            (LineKind::Deleted, Some(2), "two")
        );
        assert_eq!(
            (lines[3].kind, lines[3].new_line),
            (LineKind::Added, Some(3))
        );
        assert_eq!((lines[4].old_line, lines[4].new_line), (Some(3), Some(4)));

        let added = &files[1];
        assert_eq!(
            (added.path(), added.status, added.additions),
            ("new file.txt", FileStatus::Added, 1)
        );
        assert_eq!(added.hunks[0].lines[1].kind, LineKind::NoNewline);

        let renamed = &files[2];
        assert_eq!(
            (
                renamed.old_path.as_str(),
                renamed.new_path.as_str(),
                renamed.status
            ),
            ("old.txt", "renamed.txt", FileStatus::Renamed)
        );

        assert!(files[3].binary);
        assert_eq!(files[3].path(), "logo.png");

        let deleted = &files[4];
        assert_eq!(
            (deleted.path(), deleted.status, deleted.deletions),
            ("tab\there", FileStatus::Deleted, 1)
        );
    }
}
