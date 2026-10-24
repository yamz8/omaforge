//! Turning repository content into HTML: Markdown, syntax highlighting, and small
//! formatting helpers.

use std::{
    borrow::Cow,
    collections::HashSet,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use comrak::{nodes::NodeValue, Arena, Options};
use syntect::{
    html::{ClassStyle, ClassedHTMLGenerator},
    parsing::{SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

/// Where relative links in a rendered document point: the directory of the document,
/// at a revision of a repository.
pub struct LinkBase<'a> {
    /// `/{owner}/{repo}`
    pub repo_url: &'a str,
    pub rev: &'a str,
    /// Directory containing the document, `""` for the root.
    pub dir: &'a str,
}

impl LinkBase<'_> {
    /// Rewrite a relative URL to point into the repo: `view` is `blob` for links (the file
    /// page, which also handles directories) and `raw` for images.
    fn resolve(&self, url: &str, view: &str) -> Option<String> {
        if url.is_empty()
            || url.starts_with('#')
            || url.starts_with('/')
            || url.starts_with("//")
            || url.contains(':')
        {
            return None;
        }
        let (path, suffix) = match url.find(['?', '#']) {
            Some(i) => (&url[..i], &url[i..]),
            None => (url, ""),
        };
        let mut parts: Vec<&str> = self.dir.split('/').filter(|p| !p.is_empty()).collect();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        Some(format!(
            "{}/{view}/{}/{}{suffix}",
            self.repo_url,
            self.rev,
            parts.join("/")
        ))
    }
}

/// Render GitHub-flavored Markdown to sanitized HTML.
pub fn markdown(source: &str, base: Option<&LinkBase>) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.alerts = true;
    options.extension.header_id_prefix = Some("user-content-".into());
    options.extension.header_id_prefix_in_href = true;
    options.render.github_pre_lang = true;
    options.render.tasklist_classes = true;
    // Raw HTML is allowed through comrak and then sanitized by ammonia, as on GitHub
    // (READMEs commonly use <p align="center">, <img>, <details>...).
    options.render.r#unsafe = true;

    let arena = Arena::new();
    let root = comrak::parse_document(&arena, source, &options);
    // Highlighted code blocks would lose their classes in the sanitizer, so each one is
    // swapped for a random placeholder and put back after sanitizing. The highlighted HTML
    // is generated (and escaped) by us, not taken from the document.
    let nonce = placeholder_nonce();
    let mut highlighted = Vec::new();
    for node in root.descendants() {
        let mut data = node.data_mut();
        match &mut data.value {
            NodeValue::Link(link) => {
                if let Some(url) = base.and_then(|base| base.resolve(&link.url, "blob")) {
                    link.url = url;
                }
            }
            NodeValue::Image(image) => {
                if let Some(url) = base.and_then(|base| base.resolve(&image.url, "raw")) {
                    image.url = url;
                }
            }
            NodeValue::CodeBlock(block) => {
                let lang = block.info.split_whitespace().next().unwrap_or_default();
                if let Some(html) = highlight_snippet(lang, &block.literal) {
                    block.literal = format!("omaforge{nonce}x{}", highlighted.len());
                    highlighted.push(html);
                }
            }
            _ => {}
        }
    }
    let mut html = String::new();
    if comrak::format_html(root, &options, &mut html).is_err() {
        return String::new();
    }
    let mut html = sanitize(&html, base);
    // Replace from the last so "…x1" doesn't match inside "…x10".
    for (i, code) in highlighted.iter().enumerate().rev() {
        html = html.replacen(&format!("omaforge{nonce}x{i}"), code, 1);
    }
    html
}

fn placeholder_nonce() -> String {
    let mut bytes = [0u8; 12];
    // Without randomness, fall back to the time; the placeholder only has to be unguessable
    // enough that a document can't plant it to splice our HTML somewhere else.
    if getrandom::fill(&mut bytes).is_err() {
        bytes[..8].copy_from_slice(&now().to_le_bytes());
    }
    hex::encode(bytes)
}

/// Highlight a fenced code block whose info string names a language (`rust`, `ts`, `sh`).
fn highlight_snippet(lang: &str, code: &str) -> Option<String> {
    if lang.is_empty() || code.len() > HIGHLIGHT_LIMIT {
        return None;
    }
    let set = syntaxes();
    let syntax = set.find_syntax_by_token(lang)?;
    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        set,
        ClassStyle::SpacedPrefixed { prefix: "hl-" },
    );
    for line in LinesWithEndings::from(code) {
        generator
            .parse_html_for_line_which_includes_newline(line)
            .ok()?;
    }
    Some(generator.finalize())
}

fn sanitize(html: &str, base: Option<&LinkBase>) -> String {
    let mut builder = ammonia::Builder::default();
    builder
        .add_tags([
            "details", "summary", "input", "picture", "source", "kbd", "sup", "sub",
        ])
        .add_generic_attributes(["align", "id"])
        .add_tag_attributes("input", ["type", "checked", "disabled"])
        .add_tag_attributes("source", ["srcset", "media", "type"])
        .add_tag_attributes("img", ["width", "height", "align"])
        .add_tag_attributes("td", ["align"])
        .add_tag_attributes("th", ["align"])
        .add_tag_attributes("ol", ["start"])
        .add_tag_attributes("pre", ["lang"])
        .add_tag_attributes("a", ["aria-hidden"])
        .add_allowed_classes(
            "div",
            [
                "markdown-alert",
                "markdown-alert-note",
                "markdown-alert-tip",
                "markdown-alert-important",
                "markdown-alert-warning",
                "markdown-alert-caution",
            ],
        )
        .add_allowed_classes("p", ["markdown-alert-title"])
        .add_allowed_classes("a", ["anchor"])
        .add_allowed_classes("li", ["task-list-item"])
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            // Only ids we generated, so documents can't clobber the page's own ids.
            (_, "id") if !value.starts_with("user-content-") => None,
            ("input", "type") if value != "checkbox" => None,
            _ => Some(value.into()),
        });
    if let Some(base) = base {
        // Relative URLs that came from raw HTML (mostly <img src>); Markdown links were
        // already rewritten above, and those are absolute paths.
        builder.url_relative(ammonia::UrlRelative::Custom(Box::new(RawHtmlUrls {
            repo_url: base.repo_url.to_owned(),
            rev: base.rev.to_owned(),
            dir: base.dir.to_owned(),
        })));
    }
    builder.clean(html).to_string()
}

struct RawHtmlUrls {
    repo_url: String,
    rev: String,
    dir: String,
}

impl ammonia::UrlRelativeEvaluate<'_> for RawHtmlUrls {
    fn evaluate<'url>(&self, url: &'url str) -> Option<Cow<'url, str>> {
        let base = LinkBase {
            repo_url: &self.repo_url,
            rev: &self.rev,
            dir: &self.dir,
        };
        Some(
            base.resolve(url, "raw")
                .map_or(Cow::Borrowed(url), Cow::Owned),
        )
    }
}

fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// Load the syntax definitions now rather than on the first file view.
pub fn warm_up() {
    syntaxes();
}

fn find_syntax<'a>(
    set: &'a SyntaxSet,
    file_name: &str,
    first_line: &str,
) -> Option<&'a SyntaxReference> {
    let extension = file_name.rsplit_once('.').map(|(_, ext)| ext);
    set.find_syntax_by_extension(file_name)
        .or_else(|| extension.and_then(|ext| set.find_syntax_by_extension(ext)))
        .or_else(|| set.find_syntax_by_first_line(first_line))
}

/// Above this, files are shown without highlighting.
const HIGHLIGHT_LIMIT: usize = 512 * 1024;

/// Highlight `source` as HTML with `hl-` prefixed scope classes, returning one HTML
/// fragment per line (spans that cross lines are closed and reopened).
pub fn highlight_lines(file_name: &str, source: &str) -> Vec<String> {
    let set = syntaxes();
    let first_line = source.lines().next().unwrap_or_default();
    let syntax = (source.len() <= HIGHLIGHT_LIMIT)
        .then(|| find_syntax(set, file_name, first_line))
        .flatten();
    let Some(syntax) = syntax else {
        return source
            .lines()
            .map(|line| escape(line).into_owned())
            .collect();
    };

    let mut generator = ClassedHTMLGenerator::new_with_class_style(
        syntax,
        set,
        ClassStyle::SpacedPrefixed { prefix: "hl-" },
    );
    for line in LinesWithEndings::from(source) {
        if generator
            .parse_html_for_line_which_includes_newline(line)
            .is_err()
        {
            return source
                .lines()
                .map(|line| escape(line).into_owned())
                .collect();
        }
    }
    let html = generator.finalize();
    split_html_lines(&html)
}

/// Split highlighted HTML into lines, closing the spans open at each line end and
/// reopening them on the next line so every line is well-formed on its own.
fn split_html_lines(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut open: Vec<&str> = Vec::new();
    let mut current = String::new();
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with("<span") {
            let end = rest.find('>').map_or(rest.len(), |i| i + 1);
            open.push(&rest[..end]);
            current.push_str(&rest[..end]);
            rest = &rest[end..];
        } else if let Some(after) = rest.strip_prefix("</span>") {
            open.pop();
            current.push_str("</span>");
            rest = after;
        } else if let Some(after) = rest.strip_prefix('\n') {
            current.extend(std::iter::repeat_n("</span>", open.len()));
            lines.push(std::mem::take(&mut current));
            current.extend(open.iter().copied());
            rest = after;
        } else {
            let end = rest.find(['<', '\n']).unwrap_or(rest.len()).max(1);
            current.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    // The last line has no newline after it; keep it only if it has text.
    if !strip_tags(&current).is_empty() {
        lines.push(current);
    }
    lines
}

fn strip_tags(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    text
}

pub fn escape(text: &str) -> Cow<'_, str> {
    if !text.contains(['<', '>', '&', '"', '\'']) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

pub fn is_markdown(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    [".md", ".markdown", ".mdown", ".mkd"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Pick the README to show for a directory listing.
pub fn find_readme<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let names: Vec<&str> = names.into_iter().collect();
    let candidates: HashSet<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    for wanted in [
        "readme.md",
        "readme.markdown",
        "readme",
        "readme.txt",
        "readme.rst",
    ] {
        if candidates.contains(wanted) {
            return names.into_iter().find(|n| n.to_ascii_lowercase() == wanted);
        }
    }
    None
}

pub fn image_type(file_name: &str) -> Option<&'static str> {
    let lower = file_name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.')?.1;
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

pub fn looks_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "3 minutes ago", "yesterday", or a date for anything older than a month.
pub fn time_ago(time: i64) -> String {
    let seconds = (now() - time).max(0);
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match seconds {
        0..60 => "just now".into(),
        60..3600 => plural(seconds / 60, "minute"),
        3600..86400 => plural(seconds / 3600, "hour"),
        86400..172800 => "yesterday".into(),
        172800..2_592_000 => plural(seconds / 86400, "day"),
        _ => format!("on {}", date(time)),
    }
}

/// `Oct 3, 2025` (UTC).
pub fn date(time: i64) -> String {
    let (year, month, day) = civil_from_days(time.div_euclid(86400));
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {day}, {year}", MONTHS[(month - 1) as usize])
}

/// Full UTC timestamp for `title` tooltips.
pub fn timestamp(time: i64) -> String {
    let (year, month, day) = civil_from_days(time.div_euclid(86400));
    let secs = time.rem_euclid(86400);
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), from Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} Bytes");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "Jan 1, 1970");
        assert_eq!(date(1_761_436_800), "Oct 26, 2025");
        assert_eq!(timestamp(1_761_436_800 + 3661), "2025-10-26 01:01 UTC");
    }

    #[test]
    fn markdown_is_sanitized_and_links_rewritten() {
        let base = LinkBase {
            repo_url: "/alice/hello",
            rev: "main",
            dir: "docs",
        };
        let html = markdown(
            "# Hi\n\n[arch](architecture.md) [up](../README.md#x) [web](https://example.com)\n\n\
             ![logo](img/logo.png)\n\n<img src=\"shot.png\" onerror=\"alert(1)\">\n\n\
             <script>alert(1)</script>\n\n- [x] done\n",
            Some(&base),
        );
        assert!(
            html.contains(r#"href="/alice/hello/blob/main/docs/architecture.md""#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/alice/hello/blob/main/README.md#x""#),
            "{html}"
        );
        assert!(html.contains(r#"href="https://example.com""#), "{html}");
        assert!(
            html.contains(r#"src="/alice/hello/raw/main/docs/img/logo.png""#),
            "{html}"
        );
        assert!(
            html.contains(r#"src="/alice/hello/raw/main/docs/shot.png""#),
            "{html}"
        );
        assert!(html.contains(r#"id="user-content-hi""#), "{html}");
        assert!(html.contains(r#"type="checkbox""#), "{html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("onerror"), "{html}");
        assert!(html.contains(r#"class="task-list-item""#), "{html}");
    }

    #[test]
    fn fenced_code_is_highlighted_but_raw_html_is_not_trusted() {
        let html = markdown(
            "```rust\nfn main() {}\n```\n\n<span class=\"hl-keyword\">x</span>\n\n```\nplain <b>\n```\n",
            None,
        );
        assert!(html.contains(r#"<span class="hl-"#), "{html}");
        assert!(
            !html.contains("omaforge"),
            "placeholder left behind: {html}"
        );
        // Classes in the document's own HTML are still stripped.
        assert!(!html.contains(r#"<span class="hl-keyword">x"#), "{html}");
        assert!(html.contains("plain &lt;b&gt;"), "{html}");
    }

    #[test]
    fn highlighted_lines_are_balanced() {
        let lines = highlight_lines("main.rs", "/* a\nb */\nfn main() {}\n");
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(
                line.matches("<span").count(),
                line.matches("</span>").count(),
                "{line}"
            );
        }
        assert!(lines[2].contains("hl-"));
        assert_eq!(highlight_lines("x.unknownext", "a < b\n"), vec!["a &lt; b"]);
    }

    #[test]
    fn readme_preference() {
        assert_eq!(
            find_readme(["src", "readme.txt", "README.md"]),
            Some("README.md")
        );
        assert_eq!(find_readme(["src", "Cargo.toml"]), None);
    }
}
