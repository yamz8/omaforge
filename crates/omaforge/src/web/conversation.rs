//! The timeline shared by issues and pull requests: comments, reviews and events, shown by
//! the `timeline.html` partial.

use std::collections::HashSet;

use super::{
    render::{self, References},
    repo::Loaded,
    WebResult,
};
use crate::{db, AppState};

pub struct TimelineEntry {
    /// `comment`, `review` or `event`.
    pub kind: &'static str,
    pub id: i64,
    pub who: String,
    /// What happened, for reviews and events ("approved these changes", "closed this").
    pub text: String,
    pub html: String,
    pub time_ago: String,
    /// `approved`, `merged`, `closed`, `labeled`, ... for styling.
    pub tone: String,
    /// A link that goes with the entry: the file and line of a review comment, or the
    /// pull request that closed an issue.
    pub context: Option<(String, String)>,
}

/// Collects entries and sorts them by time.
#[derive(Default)]
pub struct Timeline(Vec<(i64, i64, TimelineEntry)>);

impl Timeline {
    pub fn push(&mut self, created: i64, entry: TimelineEntry) {
        self.0.push((created, entry.id, entry));
    }

    pub fn finish(mut self) -> Vec<TimelineEntry> {
        // Comments before events made in the same second (a comment posted while closing).
        self.0
            .sort_by_key(|(created, id, entry)| (*created, entry.kind != "comment", *id));
        self.0.into_iter().map(|(_, _, entry)| entry).collect()
    }
}

/// Existing issue numbers and usernames, so conversations link `#12` and `@name`.
pub struct Mentions {
    pub repo_url: String,
    pub issues: HashSet<i64>,
    pub users: HashSet<String>,
}

impl Mentions {
    pub async fn load(state: &AppState, repo: &Loaded) -> WebResult<Self> {
        Ok(Self {
            repo_url: repo.url.clone(),
            issues: db::issue_numbers(&state.db, repo.repo.id).await?,
            users: db::all_users(&state.db)
                .await?
                .into_iter()
                .map(|(_, name)| name)
                .collect(),
        })
    }

    pub fn render(&self, markdown: &str) -> String {
        render::conversation(
            markdown,
            &References {
                repo_url: &self.repo_url,
                issues: &self.issues,
                users: &self.users,
            },
        )
    }
}

pub fn comment(
    c: db::Comment,
    mentions: &Mentions,
    context: Option<(String, String)>,
) -> TimelineEntry {
    TimelineEntry {
        kind: "comment",
        id: c.id,
        html: mentions.render(&c.body),
        who: c.author,
        text: String::new(),
        time_ago: render::time_ago(c.created),
        tone: String::new(),
        context,
    }
}

/// Branch names, for pull request events that mention them.
pub struct Branches<'a> {
    pub base: &'a str,
    pub head: &'a str,
}

pub fn event(e: db::Event, repo_url: &str, branches: Option<&Branches>) -> TimelineEntry {
    let mut context = None;
    let text = match e.kind.as_str() {
        "merged" => format!(
            "merged commit {} into {}",
            &e.detail[..7.min(e.detail.len())],
            branches.map_or("", |b| b.base)
        ),
        "closed" => "closed this".into(),
        "reopened" => "reopened this".into(),
        "branch_deleted" => format!(
            "deleted the {} branch",
            branches.map_or(e.detail.as_str(), |b| b.head)
        ),
        "labeled" => format!("added the {} label", e.detail),
        "unlabeled" => format!("removed the {} label", e.detail),
        "assigned" if e.detail == e.actor => "self-assigned this".into(),
        "assigned" => format!("assigned {}", e.detail),
        "unassigned" => format!("unassigned {}", e.detail),
        "closed_by" => {
            context = Some((
                format!("#{}", e.detail),
                format!("{repo_url}/pull/{}", e.detail),
            ));
            "closed this in".into()
        }
        other => other.to_owned(),
    };
    TimelineEntry {
        kind: "event",
        id: e.id,
        who: e.actor,
        text,
        html: String::new(),
        time_ago: render::time_ago(e.created),
        tone: e.kind,
        context,
    }
}
