//! Repository pages: listings, the code browser, history and diffs.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::header,
    response::{IntoResponse, Redirect},
};
use forge_git::{diff::FileStatus, EntryKind, ObjectKind, Repository};
use serde::Deserialize;

use super::{
    icons, page,
    render::{self, LinkBase},
    Viewer, WebError, WebResult,
};
use crate::{auth, db, AppState};

/// Files above this are not shown inline.
const DISPLAY_LIMIT: u64 = 1024 * 1024;
/// Raw downloads are read into memory, so cap them.
const RAW_LIMIT: u64 = 100 * 1024 * 1024;
const COMMITS_PER_PAGE: usize = 30;

// ---- Shared view pieces ----

/// What every repository page shows in its header.
pub(super) struct RepoContext {
    pub(super) owner: String,
    pub(super) name: String,
    pub(super) url: String,
    pub(super) description: String,
    pub(super) is_private: bool,
    pub(super) http_clone: String,
    pub(super) ssh_clone: Option<String>,
    /// `code`, `pulls`, `commits`, `branches` or `tags`.
    pub(super) tab: &'static str,
    pub(super) open_pulls: i64,
}

pub(super) struct RefLink {
    pub(super) name: String,
    pub(super) url: String,
    pub(super) current: bool,
}

pub(super) struct RefPicker {
    pub(super) current: String,
    pub(super) branches: Vec<RefLink>,
    pub(super) tags: Vec<RefLink>,
}

pub(super) struct Crumb {
    pub(super) name: String,
    pub(super) url: String,
}

pub(super) struct CommitView {
    pub(super) id: String,
    pub(super) short_id: String,
    pub(super) subject: String,
    pub(super) author: String,
    pub(super) time_ago: String,
    pub(super) timestamp: String,
    pub(super) url: String,
    pub(super) browse_url: String,
}

/// A repository someone may view, with its git directory opened.
pub(super) struct Loaded {
    pub(super) viewer: Viewer,
    pub(super) repo: db::Repo,
    pub(super) git: Repository,
    pub(super) url: String,
    pub(super) open_pulls: i64,
}

impl Loaded {
    pub(super) fn context(&self, state: &AppState, tab: &'static str) -> RepoContext {
        RepoContext {
            owner: self.repo.owner_name.clone(),
            name: self.repo.name.clone(),
            url: self.url.clone(),
            description: self.repo.description.clone(),
            is_private: self.repo.is_private,
            http_clone: state
                .site
                .http_clone_url(&self.repo.owner_name, &self.repo.name),
            ssh_clone: state
                .site
                .ssh_clone_url(&self.repo.owner_name, &self.repo.name),
            tab,
            open_pulls: self.open_pulls,
        }
    }

    /// `{repo}/{view}/{rev}/{path}`, with the path URL-encoded.
    pub(super) fn view_url(&self, view: &str, rev: &str, path: &str) -> String {
        let mut url = format!("{}/{view}/{}", self.url, encode_path(rev));
        if !path.is_empty() {
            url.push('/');
            url.push_str(&encode_path(path));
        }
        url
    }

    pub(super) fn commit_view(&self, commit: &forge_git::Commit) -> CommitView {
        CommitView {
            id: commit.id.clone(),
            short_id: commit.short_id().to_owned(),
            subject: commit.subject.clone(),
            author: commit.author_name.clone(),
            time_ago: render::time_ago(commit.author_time),
            timestamp: render::timestamp(commit.author_time),
            url: format!("{}/commit/{}", self.url, commit.id),
            browse_url: self.view_url("tree", &commit.id, ""),
        }
    }

    /// The branch/tag picker, linking each ref to the same `view` and `path`.
    pub(super) async fn picker(
        &self,
        view: &str,
        current: &str,
        path: &str,
    ) -> WebResult<RefPicker> {
        let link = |name: String| RefLink {
            url: self.view_url(view, &name, path),
            current: name == current,
            name,
        };
        let branches = self
            .git
            .branches()
            .await?
            .into_iter()
            .map(|r| link(r.name))
            .collect();
        let tags = self
            .git
            .tags()
            .await?
            .into_iter()
            .map(|r| link(r.name))
            .collect();
        Ok(RefPicker {
            current: if current.len() == 40 {
                current[..7].to_owned()
            } else {
                current.to_owned()
            },
            branches,
            tags,
        })
    }

    pub(super) fn crumbs(&self, rev: &str, path: &str) -> Vec<Crumb> {
        let mut crumbs = Vec::new();
        let mut so_far = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(part);
            crumbs.push(Crumb {
                name: part.to_owned(),
                url: self.view_url("tree", rev, &so_far),
            });
        }
        crumbs
    }
}

/// Load a repository the viewer is allowed to see; anything else is "not found".
pub(super) async fn load(
    state: &AppState,
    viewer: Viewer,
    owner: &str,
    name: &str,
) -> WebResult<Loaded> {
    if !forge_git::is_valid_name(owner) || !forge_git::is_valid_name(name) {
        return Err(WebError::NotFound);
    }
    let repo = db::repo_by_path(&state.db, owner, name)
        .await?
        .filter(|repo| auth::can_read(viewer.user.as_ref(), repo))
        .ok_or(WebError::NotFound)?;
    let git = Repository::open(forge_git::repo_path(
        &state.repos,
        &repo.owner_name,
        &repo.name,
    ));
    let url = format!("/{}/{}", repo.owner_name, repo.name);
    let (open_pulls, _) = db::pull_counts(&state.db, repo.id).await?;
    Ok(Loaded {
        viewer,
        repo,
        git,
        url,
        open_pulls,
    })
}

/// Split `rev/path/to/file` from a URL. Branch and tag names may contain slashes, so the
/// longest prefix naming a ref wins; otherwise the first segment is a commit id.
async fn split_rev(git: &Repository, rest: &str) -> WebResult<(String, String, String)> {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err(WebError::NotFound);
    }
    let names = git.ref_names().await?;
    let split = (1..=segments.len())
        .rev()
        .find(|&n| names.contains(&segments[..n].join("/")))
        .unwrap_or(1);
    let rev = segments[..split].join("/");
    let commit = git.resolve_commit(&rev).await?.ok_or(WebError::NotFound)?;
    let path =
        forge_git::clean_path(&segments[split..].join("/")).map_err(|_| WebError::NotFound)?;
    Ok((rev, commit, path))
}

/// Percent-encode a path for use in a URL, keeping `/`.
pub(super) fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

// ---- Repository lists ----

struct RepoListItem {
    full_name: String,
    url: String,
    description: String,
    is_private: bool,
    updated: Option<String>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    viewer: Viewer,
    owner: Option<String>,
    repos: Vec<RepoListItem>,
}

async fn repo_list(
    state: &AppState,
    viewer: &Viewer,
    owner: Option<&str>,
) -> WebResult<Vec<RepoListItem>> {
    let mut items = Vec::new();
    for repo in db::visible_repos(&state.db, viewer.user.as_ref(), owner).await? {
        let git = Repository::open(forge_git::repo_path(
            &state.repos,
            &repo.owner_name,
            &repo.name,
        ));
        let updated = git
            .branches()
            .await?
            .first()
            .map(|b| render::time_ago(b.time));
        items.push(RepoListItem {
            full_name: format!("{}/{}", repo.owner_name, repo.name),
            url: format!("/{}/{}", repo.owner_name, repo.name),
            description: repo.description,
            is_private: repo.is_private,
            updated,
        });
    }
    Ok(items)
}

pub async fn index(State(state): State<AppState>, viewer: Viewer) -> WebResult {
    page(IndexPage {
        repos: repo_list(&state, &viewer, None).await?,
        viewer,
        owner: None,
    })
}

pub async fn owner(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(owner): Path<String>,
) -> WebResult {
    let user = db::user_by_username(&state.db, &owner)
        .await?
        .ok_or(WebError::NotFound)?;
    page(IndexPage {
        repos: repo_list(&state, &viewer, Some(&user.username)).await?,
        viewer,
        owner: Some(user.username),
    })
}

// ---- Code browser ----

#[derive(Template)]
#[template(path = "empty.html")]
struct EmptyPage {
    viewer: Viewer,
    repo: RepoContext,
}

struct EntryView {
    name: String,
    url: String,
    icon: &'static str,
    size: String,
}

struct ReadmeView {
    name: String,
    url: String,
    html: String,
}

#[derive(Template)]
#[template(path = "tree.html")]
struct TreePage {
    viewer: Viewer,
    repo: RepoContext,
    picker: RefPicker,
    crumbs: Vec<Crumb>,
    root_url: String,
    latest: Option<CommitView>,
    history_url: String,
    commit_count: Option<usize>,
    entries: Vec<EntryView>,
    readme: Option<ReadmeView>,
}

pub async fn home(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    if let Some(name) = name.strip_suffix(".git") {
        return Ok(Redirect::permanent(&format!("/{owner}/{name}")).into_response());
    }
    let repo = load(&state, viewer, &owner, &name).await?;
    let Some(branch) = repo.git.default_branch().await? else {
        return page(EmptyPage {
            viewer: repo.viewer.clone(),
            repo: repo.context(&state, "code"),
        });
    };
    let commit = repo
        .git
        .resolve_commit(&branch)
        .await?
        .ok_or(WebError::NotFound)?;
    tree_page(&state, &repo, &branch, &commit, "").await
}

pub async fn tree(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (rev, commit, path) = split_rev(&repo.git, &rest).await?;
    tree_page(&state, &repo, &rev, &commit, &path).await
}

async fn tree_page(
    state: &AppState,
    repo: &Loaded,
    rev: &str,
    commit: &str,
    path: &str,
) -> WebResult {
    match repo.git.object_kind(commit, path).await? {
        Some(ObjectKind::Tree) => {}
        Some(ObjectKind::Blob) => {
            return Ok(Redirect::to(&repo.view_url("blob", rev, path)).into_response())
        }
        None => return Err(WebError::NotFound),
    }
    let entries = repo.git.tree(commit, path).await?;
    let latest = repo
        .git
        .log(commit, path, 0, 1)
        .await?
        .first()
        .map(|c| repo.commit_view(c));

    let readme = match render::find_readme(
        entries
            .iter()
            .filter(|e| e.kind == EntryKind::Blob)
            .map(|e| e.name.as_str()),
    ) {
        Some(name) => {
            let file = join_path(path, name);
            let blob = repo.git.blob(commit, &file, DISPLAY_LIMIT).await?;
            blob.data.map(|data| {
                let text = String::from_utf8_lossy(&data);
                let html = if render::is_markdown(name) {
                    let base = LinkBase {
                        repo_url: &repo.url,
                        rev,
                        dir: path,
                    };
                    render::markdown(&text, Some(&base))
                } else {
                    format!("<pre>{}</pre>", render::escape(&text))
                };
                ReadmeView {
                    name: name.to_owned(),
                    url: repo.view_url("blob", rev, &file),
                    html,
                }
            })
        }
        None => None,
    };

    let entries = entries
        .into_iter()
        .map(|entry| {
            let entry_path = join_path(path, &entry.name);
            let (icon, url) = match entry.kind {
                EntryKind::Tree => (icons::FOLDER, repo.view_url("tree", rev, &entry_path)),
                EntryKind::Blob => (icons::FILE, repo.view_url("blob", rev, &entry_path)),
                EntryKind::Symlink => (icons::SYMLINK, repo.view_url("blob", rev, &entry_path)),
                // Submodules point at another repository; link to this one's view for now.
                EntryKind::Submodule => (icons::SUBMODULE, repo.view_url("tree", rev, path)),
            };
            EntryView {
                name: entry.name,
                url,
                icon,
                size: match entry.kind {
                    EntryKind::Blob => entry.size.map(render::file_size).unwrap_or_default(),
                    EntryKind::Submodule => format!("@ {}", &entry.id[..7.min(entry.id.len())]),
                    _ => String::new(),
                },
            }
        })
        .collect();

    page(TreePage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, "code"),
        picker: repo.picker("tree", rev, path).await?,
        crumbs: repo.crumbs(rev, path),
        root_url: repo.view_url("tree", rev, ""),
        latest,
        history_url: repo.view_url("commits", rev, path),
        commit_count: if path.is_empty() {
            repo.git.commit_count(commit).await.ok()
        } else {
            None
        },
        entries,
        readme,
    })
}

#[derive(Template)]
#[template(path = "blob.html")]
struct BlobPage {
    viewer: Viewer,
    repo: RepoContext,
    picker: RefPicker,
    crumbs: Vec<Crumb>,
    root_url: String,
    latest: Option<CommitView>,
    name: String,
    size: String,
    raw_url: String,
    history_url: String,
    /// Highlighted lines of a text file.
    code: Option<Vec<String>>,
    /// Rendered Markdown, plus the URL that shows its source.
    markdown: Option<(String, String)>,
    /// For rendered Markdown shown as source: the URL back to the rendered view.
    rendered_url: Option<String>,
    image_url: Option<String>,
    notice: Option<&'static str>,
}

#[derive(Deserialize)]
pub struct BlobQuery {
    plain: Option<String>,
}

pub async fn blob(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(query): Query<BlobQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (rev, commit, path) = split_rev(&repo.git, &rest).await?;
    match repo.git.object_kind(&commit, &path).await? {
        Some(ObjectKind::Blob) => {}
        Some(ObjectKind::Tree) => {
            return Ok(Redirect::to(&repo.view_url("tree", &rev, &path)).into_response())
        }
        None => return Err(WebError::NotFound),
    }
    let file_name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    let blob = repo.git.blob(&commit, &path, DISPLAY_LIMIT).await?;
    let raw_url = repo.view_url("raw", &rev, &path);
    let blob_url = repo.view_url("blob", &rev, &path);
    let plain = query.plain.is_some_and(|v| v != "0");

    let mut view = BlobPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "code"),
        picker: repo.picker("blob", &rev, &path).await?,
        crumbs: repo.crumbs(&rev, parent_dir(&path)),
        root_url: repo.view_url("tree", &rev, ""),
        latest: repo
            .git
            .log(&commit, &path, 0, 1)
            .await?
            .first()
            .map(|c| repo.commit_view(c)),
        name: file_name.clone(),
        size: render::file_size(blob.size),
        raw_url: raw_url.clone(),
        history_url: repo.view_url("commits", &rev, &path),
        code: None,
        markdown: None,
        rendered_url: None,
        image_url: None,
        notice: None,
    };

    if let Some(_mime) = render::image_type(&file_name) {
        view.image_url = Some(raw_url);
    } else if let Some(data) = blob.data {
        if render::looks_binary(&data) {
            view.notice = Some("Binary file not shown.");
        } else {
            let text = String::from_utf8_lossy(&data);
            if render::is_markdown(&file_name) && !plain {
                let base = LinkBase {
                    repo_url: &repo.url,
                    rev: &rev,
                    dir: parent_dir(&path),
                };
                view.markdown = Some((
                    render::markdown(&text, Some(&base)),
                    format!("{blob_url}?plain=1"),
                ));
            } else {
                if render::is_markdown(&file_name) {
                    view.rendered_url = Some(blob_url);
                }
                view.code = Some(render::highlight_lines(&file_name, &text));
            }
        }
    } else {
        view.notice = Some("This file is too large to display.");
    }
    page(view)
}

pub async fn raw(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (_rev, commit, path) = split_rev(&repo.git, &rest).await?;
    if repo.git.object_kind(&commit, &path).await? != Some(ObjectKind::Blob) {
        return Err(WebError::NotFound);
    }
    let blob = repo.git.blob(&commit, &path, RAW_LIMIT).await?;
    let Some(data) = blob.data else {
        return Ok((
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "file too large\n",
        )
            .into_response());
    };
    let file_name = path.rsplit('/').next().unwrap_or(&path);
    let content_type = match render::image_type(file_name) {
        Some(mime) => mime,
        None if render::looks_binary(&data) || std::str::from_utf8(&data).is_err() => {
            "application/octet-stream"
        }
        None => "text/plain; charset=utf-8",
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Repository content must never run as part of this site (e.g. an .svg or a
            // file a browser might sniff as HTML).
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox",
            ),
        ],
        data,
    )
        .into_response())
}

// ---- History ----

struct CommitGroup {
    date: String,
    commits: Vec<CommitView>,
}

#[derive(Template)]
#[template(path = "commits.html")]
struct CommitsPage {
    viewer: Viewer,
    repo: RepoContext,
    picker: RefPicker,
    path: String,
    path_url: String,
    groups: Vec<CommitGroup>,
    newer_url: Option<String>,
    older_url: Option<String>,
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<usize>,
}

pub async fn commits_default(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    match repo.git.default_branch().await? {
        Some(branch) => Ok(Redirect::to(&repo.view_url("commits", &branch, "")).into_response()),
        None => page(EmptyPage {
            viewer: repo.viewer.clone(),
            repo: repo.context(&state, "commits"),
        }),
    }
}

pub async fn commits(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(query): Query<PageQuery>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let (rev, commit, path) = split_rev(&repo.git, &rest).await?;
    let page_number = query.page.unwrap_or(1).max(1);
    let mut history = repo
        .git
        .log(
            &commit,
            &path,
            (page_number - 1) * COMMITS_PER_PAGE,
            COMMITS_PER_PAGE + 1,
        )
        .await?;
    let has_older = history.len() > COMMITS_PER_PAGE;
    history.truncate(COMMITS_PER_PAGE);

    let mut groups: Vec<CommitGroup> = Vec::new();
    for commit in &history {
        let date = render::date(commit.author_time);
        let view = repo.commit_view(commit);
        match groups.last_mut() {
            Some(group) if group.date == date => group.commits.push(view),
            _ => groups.push(CommitGroup {
                date,
                commits: vec![view],
            }),
        }
    }
    let base = repo.view_url("commits", &rev, &path);
    page(CommitsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "commits"),
        picker: repo.picker("commits", &rev, &path).await?,
        path_url: repo.view_url("tree", &rev, &path),
        path,
        groups,
        newer_url: (page_number > 1).then(|| format!("{base}?page={}", page_number - 1)),
        older_url: has_older.then(|| format!("{base}?page={}", page_number + 1)),
    })
}

#[derive(Template)]
#[template(path = "commit.html")]
struct CommitPage {
    viewer: Viewer,
    repo: RepoContext,
    commit: CommitView,
    body: String,
    committer: Option<String>,
    parents: Vec<(String, String)>,
    files: Vec<super::diff::FileView>,
    additions: usize,
    deletions: usize,
}

pub async fn commit(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let commit_id = repo
        .git
        .resolve_commit(&id)
        .await?
        .ok_or(WebError::NotFound)?;
    let commit = repo
        .git
        .commit(&commit_id)
        .await?
        .ok_or(WebError::NotFound)?;
    let diff = repo.git.diff(&commit_id).await?;

    let files = super::diff::file_views(&diff, |file| {
        if file.status == FileStatus::Deleted {
            commit.parents.first().map_or_else(String::new, |parent| {
                repo.view_url("blob", parent, file.path())
            })
        } else {
            repo.view_url("blob", &commit.id, file.path())
        }
    });

    page(CommitPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(&state, "commits"),
        commit: repo.commit_view(&commit),
        body: commit.body.clone(),
        committer: (commit.committer_name != commit.author_name)
            .then(|| commit.committer_name.clone()),
        parents: commit
            .parents
            .iter()
            .map(|p| {
                (
                    p[..7.min(p.len())].to_owned(),
                    format!("{}/commit/{p}", repo.url),
                )
            })
            .collect(),
        additions: files.iter().map(|f| f.additions).sum(),
        deletions: files.iter().map(|f| f.deletions).sum(),
        files,
    })
}

// ---- Branches and tags ----

struct RefView {
    name: String,
    url: String,
    commit_url: String,
    short_id: String,
    subject: String,
    time_ago: String,
    is_default: bool,
}

#[derive(Template)]
#[template(path = "refs.html")]
struct RefsPage {
    viewer: Viewer,
    repo: RepoContext,
    kind: &'static str,
    refs: Vec<RefView>,
}

pub async fn branches(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let default = repo.git.default_branch().await?;
    let refs = repo.git.branches().await?;
    refs_page(&state, &repo, "branches", refs, default.as_deref())
}

pub async fn tags(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> WebResult {
    let repo = load(&state, viewer, &owner, &name).await?;
    let refs = repo.git.tags().await?;
    refs_page(&state, &repo, "tags", refs, None)
}

fn refs_page(
    state: &AppState,
    repo: &Loaded,
    kind: &'static str,
    refs: Vec<forge_git::Ref>,
    default: Option<&str>,
) -> WebResult {
    let refs = refs
        .into_iter()
        .map(|r| RefView {
            url: repo.view_url("tree", &r.name, ""),
            commit_url: format!("{}/commit/{}", repo.url, r.commit),
            short_id: r.commit[..7.min(r.commit.len())].to_owned(),
            time_ago: render::time_ago(r.time),
            is_default: Some(r.name.as_str()) == default,
            subject: r.subject,
            name: r.name,
        })
        .collect();
    page(RefsPage {
        viewer: repo.viewer.clone(),
        repo: repo.context(state, kind),
        kind,
        refs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(encode_path("docs/a b#c?.md"), "docs/a%20b%23c%3F.md");
        assert_eq!(encode_path("ünï"), "%C3%BCn%C3%AF");
    }
}
