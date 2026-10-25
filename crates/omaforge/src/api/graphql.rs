//! The subset of GitHub's GraphQL schema that `gh` uses, with GitHub's names, so the CLI
//! works unchanged. Things omaforge doesn't have (reactions, milestones, checks, teams)
//! exist in the schema but are always empty, which is what `gh` expects of a repository
//! that doesn't use them.

use std::collections::HashMap;

use async_graphql::{
    Context, EmptySubscription, Enum, Error, ErrorExtensions, InputObject, Object, Result,
    SimpleObject, Union, ID,
};
use forge_git::{Mergeability, Repository};

use crate::{
    actions,
    auth::{self, Access},
    ci, db, events, web, AppState,
};

pub type Schema = async_graphql::Schema<Query, Mutation, EmptySubscription>;

pub fn schema() -> Schema {
    Schema::build(Query, Mutation, EmptySubscription)
        .limit_depth(32)
        .limit_complexity(50_000)
        .finish()
}

/// Per-request data.
pub struct ApiContext {
    pub state: AppState,
    pub viewer: Option<db::User>,
}

fn api<'a>(ctx: &'a Context<'_>) -> &'a ApiContext {
    ctx.data_unchecked::<ApiContext>()
}

fn not_found(what: &str) -> Error {
    Error::new(what.to_owned()).extend_with(|_, e| e.set("type", "NOT_FOUND"))
}

fn forbidden(what: &str) -> Error {
    Error::new(what.to_owned()).extend_with(|_, e| e.set("type", "FORBIDDEN"))
}

fn internal(error: anyhow::Error) -> Error {
    tracing::error!(error = format!("{error:#}"), "GraphQL request failed");
    Error::new("Something went wrong")
}

trait OrInternal<T> {
    fn or_internal(self) -> Result<T>;
}

impl<T> OrInternal<T> for anyhow::Result<T> {
    fn or_internal(self) -> Result<T> {
        self.map_err(internal)
    }
}

/// `2026-10-25T01:02:03Z`
fn iso(time: i64) -> String {
    web::render::iso8601(time)
}

// ---- IDs ----
//
// Global ids are a prefix and a database id, like GitHub's (`R_…` for repositories).

fn parse_id(id: &ID, prefix: &str) -> Result<i64> {
    id.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| {
            not_found(&format!(
                "Could not resolve to a node with the global id of '{}'",
                id.as_str()
            ))
        })
}

// ---- Enums ----

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum IssueStateReason {
    Completed,
    NotPlanned,
    Reopened,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum IssueClosedStateReason {
    Completed,
    NotPlanned,
    Duplicate,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum MergeStateStatus {
    Behind,
    Blocked,
    Clean,
    Dirty,
    Draft,
    HasHooks,
    Unknown,
    Unstable,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum RepositoryPermission {
    Admin,
    Maintain,
    Write,
    Triage,
    Read,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestReviewState {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestReviewDecision {
    ChangesRequested,
    Approved,
    ReviewRequired,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestMergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum CommentAuthorAssociation {
    Owner,
    Collaborator,
    Contributor,
    #[graphql(name = "NONE")]
    NoAssociation,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum ReactionContent {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum StatusState {
    Error,
    Expected,
    Failure,
    Pending,
    Success,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum CheckStatusState {
    Completed,
    InProgress,
    Pending,
    Queued,
    Requested,
    Waiting,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum CheckConclusionState {
    ActionRequired,
    Cancelled,
    Failure,
    Neutral,
    Skipped,
    Stale,
    StartupFailure,
    Success,
    TimedOut,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum IssueOrderField {
    CreatedAt,
    UpdatedAt,
    Comments,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum LabelOrderField {
    Name,
    CreatedAt,
}

#[derive(InputObject)]
pub struct LabelOrder {
    pub field: LabelOrderField,
    pub direction: OrderDirection,
}

#[derive(InputObject)]
pub struct IssueOrder {
    pub field: IssueOrderField,
    pub direction: OrderDirection,
}

#[derive(InputObject, Default)]
pub struct IssueFilters {
    pub assignee: Option<String>,
    pub created_by: Option<String>,
    pub mentioned: Option<String>,
    pub labels: Option<Vec<String>>,
    pub milestone: Option<String>,
    pub milestone_number: Option<String>,
    pub states: Option<Vec<IssueState>>,
    pub since: Option<String>,
    pub viewer_subscribed: Option<bool>,
}

// ---- Connections ----

#[derive(SimpleObject, Default)]
pub struct PageInfo {
    has_next_page: bool,
    has_previous_page: bool,
    start_cursor: Option<String>,
    end_cursor: Option<String>,
}

/// Take a page of `items`: cursors are positions in the full list.
fn paginate<T>(
    mut items: Vec<T>,
    first: Option<i32>,
    last: Option<i32>,
    after: Option<&str>,
) -> (Vec<T>, PageInfo, i64) {
    let total = items.len() as i64;
    let start = after
        .and_then(|c| c.parse::<usize>().ok())
        .map_or(0, |n| n + 1)
        .min(items.len());
    items.drain(..start);
    let mut offset = start;
    if let Some(last) = last.filter(|n| *n >= 0) {
        let skip = items.len().saturating_sub(last as usize);
        items.drain(..skip);
        offset += skip;
    }
    let mut has_next_page = false;
    if let Some(first) = first.filter(|n| *n >= 0) {
        has_next_page = items.len() > first as usize;
        items.truncate(first as usize);
    }
    let page = PageInfo {
        has_next_page,
        has_previous_page: offset > 0,
        start_cursor: (!items.is_empty()).then(|| offset.to_string()),
        end_cursor: (!items.is_empty()).then(|| (offset + items.len() - 1).to_string()),
    };
    (items, page, total)
}

/// A GitHub-style connection: `totalCount`, `nodes`, `edges { cursor node }` and
/// `pageInfo`.
macro_rules! connection {
    ($name:ident, $edge:ident, $node:ty) => {
        #[derive(SimpleObject, Clone)]
        pub struct $edge {
            cursor: String,
            node: $node,
        }

        #[derive(SimpleObject)]
        pub struct $name {
            total_count: i64,
            nodes: Vec<$node>,
            edges: Vec<$edge>,
            page_info: PageInfo,
        }

        impl $name {
            fn page(
                items: Vec<$node>,
                first: Option<i32>,
                last: Option<i32>,
                after: Option<&str>,
            ) -> Self {
                let (nodes, page_info, total_count) = paginate(items, first, last, after);
                let start = page_info
                    .start_cursor
                    .as_deref()
                    .and_then(|c| c.parse::<usize>().ok())
                    .unwrap_or(0);
                let edges = nodes
                    .iter()
                    .enumerate()
                    .map(|(i, node)| $edge {
                        cursor: (start + i).to_string(),
                        node: node.clone(),
                    })
                    .collect();
                Self {
                    total_count,
                    nodes,
                    edges,
                    page_info,
                }
            }
        }
    };
}

// ---- Users and other actors ----

#[derive(SimpleObject, Clone)]
pub struct User {
    id: ID,
    database_id: i64,
    login: String,
    name: String,
    url: String,
    avatar_url: String,
    email: String,
}

fn user_node(api: &ApiContext, u: &db::User) -> User {
    user(&api.state, u.id, &u.username)
}

fn user(state: &AppState, id: i64, login: &str) -> User {
    User {
        id: ID(format!("U_{id}")),
        database_id: id,
        login: login.to_owned(),
        name: login.to_owned(),
        url: format!("{}/{login}", state.site.http_url),
        avatar_url: String::new(),
        email: String::new(),
    }
}

#[derive(SimpleObject, Clone)]
pub struct Bot {
    login: String,
}

#[derive(SimpleObject, Clone)]
pub struct Organization {
    login: String,
}

#[derive(SimpleObject, Clone)]
pub struct Team {
    name: String,
    slug: String,
    organization: Organization,
}

#[derive(Union, Clone)]
pub enum RequestedReviewer {
    User(User),
    Bot(Bot),
    Team(Team),
}

#[derive(SimpleObject, Clone)]
pub struct ReviewRequest {
    requested_reviewer: Option<RequestedReviewer>,
}

connection!(ReviewRequestConnection, ReviewRequestEdge, ReviewRequest);
connection!(UserConnection, UserEdge, User);

#[derive(SimpleObject, Clone)]
pub struct ReactingUserConnection {
    total_count: i64,
}

#[derive(SimpleObject, Clone)]
pub struct ReactionGroup {
    content: ReactionContent,
    users: ReactingUserConnection,
}

#[derive(SimpleObject)]
pub struct Milestone {
    number: i64,
    title: String,
    description: String,
    due_on: Option<String>,
}

#[derive(SimpleObject)]
pub struct AutoMergeRequest {
    author_email: Option<String>,
    commit_body: Option<String>,
    commit_headline: Option<String>,
    merge_method: PullRequestMergeMethod,
    enabled_at: Option<String>,
    enabled_by: Option<User>,
}

// ---- Projects, issue types and sub-issues (not in omaforge; always empty) ----

#[derive(SimpleObject, Clone)]
pub struct ProjectV2 {
    id: ID,
    title: String,
}

#[derive(SimpleObject, Clone)]
pub struct ProjectV2ItemFieldSingleSelectValue {
    option_id: Option<String>,
    name: Option<String>,
}

#[derive(SimpleObject, Clone)]
pub struct ProjectV2ItemFieldTextValue {
    text: Option<String>,
}

#[derive(Union, Clone)]
pub enum ProjectV2ItemFieldValue {
    SingleSelect(ProjectV2ItemFieldSingleSelectValue),
    Text(ProjectV2ItemFieldTextValue),
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct ProjectV2Item {
    id: ID,
    project: ProjectV2,
}

#[async_graphql::ComplexObject]
impl ProjectV2Item {
    async fn field_value_by_name(&self, name: String) -> Option<ProjectV2ItemFieldValue> {
        let _ = name;
        None
    }
}

connection!(ProjectV2ItemConnection, ProjectV2ItemEdge, ProjectV2Item);

#[derive(SimpleObject, Clone)]
pub struct IssueType {
    id: ID,
    name: String,
    description: Option<String>,
    color: String,
}

#[derive(SimpleObject, Clone)]
pub struct SubIssuesSummary {
    total: i64,
    completed: i64,
    percent_completed: i64,
}

// ---- Labels ----

#[derive(SimpleObject, Clone)]
pub struct Label {
    id: ID,
    name: String,
    description: String,
    /// Hex without `#`, like GitHub.
    color: String,
    url: String,
}

connection!(LabelConnection, LabelEdge, Label);

fn label(repo_url: &str, l: &db::Label) -> Label {
    Label {
        id: ID(format!("LA_{}", l.id)),
        name: l.name.clone(),
        description: l.description.clone(),
        color: l.color.trim_start_matches('#').to_owned(),
        url: format!("{repo_url}/issues?label={}", web::form_urlencode(&l.name)),
    }
}

// ---- Commits and checks ----

#[derive(SimpleObject, Clone)]
pub struct Workflow {
    name: String,
}

#[derive(SimpleObject, Clone)]
pub struct WorkflowRun {
    workflow: Workflow,
}

#[derive(SimpleObject, Clone)]
pub struct CheckSuite {
    workflow_run: Option<WorkflowRun>,
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct CheckRun {
    name: String,
    check_suite: Option<CheckSuite>,
    status: CheckStatusState,
    conclusion: Option<CheckConclusionState>,
    started_at: Option<String>,
    completed_at: Option<String>,
    details_url: Option<String>,
    #[graphql(skip)]
    repo_id: i64,
}

/// Whether a pull request's base branch requires checks to pass (then every check is
/// required; omaforge doesn't name individual required checks).
async fn checks_required(
    ctx: &Context<'_>,
    repo_id: i64,
    pull_request_id: Option<ID>,
    pull_request_number: Option<i64>,
) -> Result<bool> {
    let db = &api(ctx).state.db;
    let number = match (pull_request_number, pull_request_id) {
        (Some(n), _) => n,
        (None, Some(id)) => {
            let issue_id = parse_id(&id, "PR")?;
            match db::pull_by_issue_id(db, issue_id).await.or_internal()? {
                Some(pull) => pull.number,
                None => return Ok(false),
            }
        }
        (None, None) => return Ok(false),
    };
    let Some(pull) = db::pull(db, repo_id, number).await.or_internal()? else {
        return Ok(false);
    };
    let rules = db::branch_rules(db, repo_id).await.or_internal()?;
    Ok(db::rule_for_branch(&rules, &pull.base_branch).is_some_and(|r| r.require_checks))
}

#[async_graphql::ComplexObject]
impl CheckRun {
    async fn is_required(
        &self,
        ctx: &Context<'_>,
        pull_request_id: Option<ID>,
        pull_request_number: Option<i64>,
    ) -> Result<bool> {
        checks_required(ctx, self.repo_id, pull_request_id, pull_request_number).await
    }
}

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct StatusContext {
    context: String,
    state: StatusState,
    target_url: Option<String>,
    created_at: String,
    description: Option<String>,
    #[graphql(skip)]
    repo_id: i64,
}

#[async_graphql::ComplexObject]
impl StatusContext {
    async fn is_required(
        &self,
        ctx: &Context<'_>,
        pull_request_id: Option<ID>,
        pull_request_number: Option<i64>,
    ) -> Result<bool> {
        checks_required(ctx, self.repo_id, pull_request_id, pull_request_number).await
    }
}

#[derive(Union, Clone)]
pub enum StatusCheckRollupContext {
    StatusContext(StatusContext),
    CheckRun(CheckRun),
}

connection!(
    StatusCheckRollupContextConnection,
    StatusCheckRollupContextEdge,
    StatusCheckRollupContext
);

pub struct StatusCheckRollup {
    state: StatusState,
    contexts: Vec<StatusCheckRollupContext>,
}

#[Object]
impl StatusCheckRollup {
    async fn state(&self) -> StatusState {
        self.state
    }

    async fn contexts(
        &self,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
    ) -> StatusCheckRollupContextConnection {
        StatusCheckRollupContextConnection::page(
            self.contexts.clone(),
            first,
            last,
            after.as_deref(),
        )
    }
}

/// CI jobs (as check runs) and API-reported statuses for a commit.
async fn rollup(
    state: &AppState,
    repo: &db::Repo,
    sha: &str,
) -> anyhow::Result<Option<StatusCheckRollup>> {
    let checks = ci::Checks::load(state, repo.id, sha).await?;
    let Some(summary) = checks.summary() else {
        return Ok(None);
    };
    let repo_url = format!("{}/{}/{}", state.site.http_url, repo.owner_name, repo.name);
    let mut contexts = Vec::new();
    let check_run = |name: String,
                     run: &db::Run,
                     state_name: &str,
                     status_name: &str,
                     started: Option<i64>,
                     completed: Option<i64>,
                     url: String| {
        let (status, conclusion) = match status_name {
            "completed" => (
                CheckStatusState::Completed,
                Some(match state_name {
                    "success" => CheckConclusionState::Success,
                    "cancelled" => CheckConclusionState::Cancelled,
                    "skipped" => CheckConclusionState::Skipped,
                    _ => CheckConclusionState::Failure,
                }),
            ),
            "in_progress" => (CheckStatusState::InProgress, None),
            "waiting" => (CheckStatusState::Waiting, None),
            _ => (CheckStatusState::Queued, None),
        };
        StatusCheckRollupContext::CheckRun(CheckRun {
            name,
            check_suite: Some(CheckSuite {
                workflow_run: Some(WorkflowRun {
                    workflow: Workflow {
                        name: run.workflow_name.clone(),
                    },
                }),
            }),
            status,
            conclusion,
            started_at: started.map(iso),
            completed_at: completed.map(iso),
            details_url: Some(url),
            repo_id: repo.id,
        })
    };
    for (run, jobs) in &checks.runs {
        let run_url = format!("{repo_url}/actions/runs/{}", run.id);
        if jobs.is_empty() {
            contexts.push(check_run(
                run.workflow_name.clone(),
                run,
                run.state(),
                &run.status,
                run.started,
                run.completed,
                run_url.clone(),
            ));
        }
        for job in jobs {
            contexts.push(check_run(
                job.name.clone(),
                run,
                job.state(),
                &job.status,
                job.started,
                job.completed,
                format!("{run_url}/jobs/{}", job.id),
            ));
        }
    }
    for status in &checks.statuses {
        contexts.push(StatusCheckRollupContext::StatusContext(StatusContext {
            context: status.context.clone(),
            state: match status.state.as_str() {
                "success" => StatusState::Success,
                "failure" => StatusState::Failure,
                "error" => StatusState::Error,
                _ => StatusState::Pending,
            },
            target_url: (!status.target_url.is_empty()).then(|| status.target_url.clone()),
            created_at: iso(status.created),
            description: (!status.description.is_empty()).then(|| status.description.clone()),
            repo_id: repo.id,
        }));
    }
    Ok(Some(StatusCheckRollup {
        state: match summary {
            "success" => StatusState::Success,
            "failure" => StatusState::Failure,
            _ => StatusState::Pending,
        },
        contexts,
    }))
}

#[derive(Clone)]
pub struct Commit {
    commit: forge_git::Commit,
    url: String,
    repo: db::Repo,
}

#[Object]
impl Commit {
    async fn oid(&self) -> &str {
        &self.commit.id
    }

    async fn abbreviated_oid(&self) -> &str {
        self.commit.short_id()
    }

    async fn message_headline(&self) -> &str {
        &self.commit.subject
    }

    async fn message_body(&self) -> &str {
        &self.commit.body
    }

    async fn authored_date(&self) -> String {
        iso(self.commit.author_time)
    }

    async fn committed_date(&self) -> String {
        iso(self.commit.committer_time)
    }

    async fn url(&self) -> &str {
        &self.url
    }

    async fn status_check_rollup(&self, ctx: &Context<'_>) -> Result<Option<StatusCheckRollup>> {
        rollup(&api(ctx).state, &self.repo, &self.commit.id)
            .await
            .or_internal()
    }
}

#[derive(SimpleObject, Clone)]
pub struct PullRequestCommit {
    commit: Commit,
}

connection!(
    PullRequestCommitConnection,
    PullRequestCommitEdge,
    PullRequestCommit
);

// ---- Repository ----

/// A repository the viewer may read, with its git directory.
#[derive(Clone)]
pub struct RepositoryNode {
    repo: db::Repo,
    access: Access,
}

impl RepositoryNode {
    fn git(&self, state: &AppState) -> Repository {
        Repository::open(forge_git::repo_path(
            &state.repos,
            &self.repo.owner_name,
            &self.repo.name,
        ))
    }

    fn web_url(&self, state: &AppState) -> String {
        format!(
            "{}/{}/{}",
            state.site.http_url, self.repo.owner_name, self.repo.name
        )
    }
}

/// Load a repository the viewer can read. Others look missing, as on the web.
async fn repository(api: &ApiContext, owner: &str, name: &str) -> Result<Option<RepositoryNode>> {
    let not_found = || {
        not_found(&format!(
            "Could not resolve to a Repository with the name '{owner}/{name}'."
        ))
    };
    let Some(repo) = db::repo_by_path(&api.state.db, owner, name)
        .await
        .or_internal()?
    else {
        return Err(not_found());
    };
    let access = auth::access(&api.state.db, api.viewer.as_ref(), &repo)
        .await
        .or_internal()?;
    if access < Access::Read {
        return Err(not_found());
    }
    Ok(Some(RepositoryNode { repo, access }))
}

async fn repository_by_id(api: &ApiContext, id: &ID) -> Result<RepositoryNode> {
    let repo_id = parse_id(id, "R")?;
    let repo = db::repo_by_id(&api.state.db, repo_id)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to a Repository"))?;
    let access = auth::access(&api.state.db, api.viewer.as_ref(), &repo)
        .await
        .or_internal()?;
    if access < Access::Read {
        return Err(not_found("Could not resolve to a Repository"));
    }
    Ok(RepositoryNode { repo, access })
}

#[derive(SimpleObject, Clone)]
pub struct BranchProtectionRule {
    requires_strict_status_checks: bool,
    requires_approving_reviews: bool,
    required_approving_review_count: Option<i64>,
}

#[derive(SimpleObject)]
#[graphql(complex)]
pub struct Ref {
    name: String,
    prefix: String,
    #[graphql(skip)]
    repo_id: i64,
}

#[async_graphql::ComplexObject]
impl Ref {
    async fn branch_protection_rule(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<BranchProtectionRule>> {
        let rules = db::branch_rules(&api(ctx).state.db, self.repo_id)
            .await
            .or_internal()?;
        Ok(
            db::rule_for_branch(&rules, &self.name).map(|r| BranchProtectionRule {
                requires_strict_status_checks: false,
                requires_approving_reviews: r.required_approvals > 0,
                required_approving_review_count: Some(r.required_approvals),
            }),
        )
    }
}

#[derive(Union)]
pub enum IssueOrPullRequest {
    Issue(IssueNode),
    PullRequest(PullRequestNode),
}

#[Object(name = "Repository")]
impl RepositoryNode {
    async fn id(&self) -> ID {
        ID(format!("R_{}", self.repo.id))
    }

    async fn database_id(&self) -> i64 {
        self.repo.id
    }

    async fn name(&self) -> &str {
        &self.repo.name
    }

    async fn name_with_owner(&self) -> String {
        format!("{}/{}", self.repo.owner_name, self.repo.name)
    }

    async fn owner(&self, ctx: &Context<'_>) -> User {
        user(&api(ctx).state, self.repo.owner_id, &self.repo.owner_name)
    }

    async fn description(&self) -> &str {
        &self.repo.description
    }

    async fn url(&self, ctx: &Context<'_>) -> String {
        self.web_url(&api(ctx).state)
    }

    async fn ssh_url(&self, ctx: &Context<'_>) -> String {
        let state = &api(ctx).state;
        state
            .site
            .ssh_clone_url(&self.repo.owner_name, &self.repo.name)
            .unwrap_or_else(|| {
                state
                    .site
                    .http_clone_url(&self.repo.owner_name, &self.repo.name)
            })
    }

    async fn is_private(&self) -> bool {
        self.repo.is_private
    }

    async fn visibility(&self) -> &str {
        if self.repo.is_private {
            "PRIVATE"
        } else {
            "PUBLIC"
        }
    }

    async fn is_fork(&self) -> bool {
        false
    }

    async fn is_archived(&self) -> bool {
        false
    }

    async fn is_empty(&self, ctx: &Context<'_>) -> Result<bool> {
        self.git(&api(ctx).state).is_empty().await.or_internal()
    }

    async fn has_issues_enabled(&self) -> bool {
        true
    }

    async fn has_wiki_enabled(&self) -> bool {
        false
    }

    async fn has_projects_enabled(&self) -> bool {
        false
    }

    async fn has_discussions_enabled(&self) -> bool {
        false
    }

    async fn merge_commit_allowed(&self) -> bool {
        true
    }

    async fn squash_merge_allowed(&self) -> bool {
        true
    }

    async fn rebase_merge_allowed(&self) -> bool {
        true
    }

    async fn delete_branch_on_merge(&self) -> bool {
        false
    }

    async fn viewer_permission(&self) -> Option<RepositoryPermission> {
        match self.access {
            Access::Admin => Some(RepositoryPermission::Admin),
            Access::Write => Some(RepositoryPermission::Write),
            Access::Read => Some(RepositoryPermission::Read),
            Access::None => None,
        }
    }

    async fn default_branch_ref(&self, ctx: &Context<'_>) -> Result<Option<Ref>> {
        let branch = self
            .git(&api(ctx).state)
            .default_branch()
            .await
            .or_internal()?;
        Ok(branch.map(|name| Ref {
            name,
            prefix: "refs/heads/".into(),
            repo_id: self.repo.id,
        }))
    }

    /// omaforge has no forks yet.
    async fn parent(&self) -> Option<RepositoryNode> {
        None
    }

    #[allow(clippy::too_many_arguments)]
    async fn pull_requests(
        &self,
        ctx: &Context<'_>,
        states: Option<Vec<PullRequestState>>,
        base_ref_name: Option<String>,
        head_ref_name: Option<String>,
        labels: Option<Vec<String>>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
        order_by: Option<IssueOrder>,
    ) -> Result<PullRequestConnection> {
        let state = &api(ctx).state;
        let states = states.unwrap_or_else(|| {
            vec![
                PullRequestState::Open,
                PullRequestState::Closed,
                PullRequestState::Merged,
            ]
        });
        let mut pulls = Vec::new();
        for (db_state, wanted) in [
            ("open", PullRequestState::Open),
            ("closed", PullRequestState::Closed),
        ] {
            let want_closed = states.contains(&PullRequestState::Closed);
            let want_merged = states.contains(&PullRequestState::Merged);
            if wanted == PullRequestState::Open && !states.contains(&PullRequestState::Open) {
                continue;
            }
            if wanted == PullRequestState::Closed && !want_closed && !want_merged {
                continue;
            }
            for pull in db::pulls(&state.db, self.repo.id, db_state)
                .await
                .or_internal()?
            {
                let keep = match pull_state(&pull) {
                    PullRequestState::Open => true,
                    PullRequestState::Merged => want_merged,
                    PullRequestState::Closed => want_closed,
                };
                if keep {
                    pulls.push(pull);
                }
            }
        }
        pulls.retain(|p| base_ref_name.as_ref().is_none_or(|b| &p.base_branch == b));
        pulls.retain(|p| head_ref_name.as_ref().is_none_or(|h| &p.head_branch == h));
        if let Some(labels) = labels.filter(|l| !l.is_empty()) {
            let by_issue = db::labels_by_issue(&state.db, self.repo.id)
                .await
                .or_internal()?;
            pulls.retain(|p| {
                let names: Vec<&str> = by_issue.get(&p.issue_id).map_or(Vec::new(), |ls| {
                    ls.iter().map(|l| l.name.as_str()).collect()
                });
                labels
                    .iter()
                    .all(|l| names.iter().any(|n| n.eq_ignore_ascii_case(l)))
            });
        }
        sort_by(
            &mut pulls,
            order_by.as_ref(),
            |p| p.created,
            |p| p.number,
            |p| p.comment_count,
        );
        let nodes = pulls
            .into_iter()
            .map(|pull| PullRequestNode {
                pull,
                repo: self.clone(),
            })
            .collect();
        Ok(PullRequestConnection::page(
            nodes,
            first,
            last,
            after.as_deref(),
        ))
    }

    async fn pull_request(
        &self,
        ctx: &Context<'_>,
        number: i64,
    ) -> Result<Option<PullRequestNode>> {
        let pull = db::pull(&api(ctx).state.db, self.repo.id, number)
            .await
            .or_internal()?;
        match pull {
            Some(pull) => Ok(Some(PullRequestNode {
                pull,
                repo: self.clone(),
            })),
            None => Err(not_found(&format!(
                "Could not resolve to a PullRequest with the number of {number}."
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn issues(
        &self,
        ctx: &Context<'_>,
        states: Option<Vec<IssueState>>,
        labels: Option<Vec<String>>,
        filter_by: Option<IssueFilters>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
        order_by: Option<IssueOrder>,
    ) -> Result<IssueConnection> {
        let state = &api(ctx).state;
        let filter = filter_by.unwrap_or_default();
        let states = states
            .or(filter.states.clone())
            .unwrap_or_else(|| vec![IssueState::Open, IssueState::Closed]);
        let mut issues = Vec::new();
        for (db_state, wanted) in [("open", IssueState::Open), ("closed", IssueState::Closed)] {
            if states.contains(&wanted) {
                issues.extend(
                    db::issues(&state.db, self.repo.id, db_state, None, None)
                        .await
                        .or_internal()?,
                );
            }
        }
        let labels: Vec<String> = labels.into_iter().chain(filter.labels).flatten().collect();
        if !labels.is_empty() {
            let by_issue = db::labels_by_issue(&state.db, self.repo.id)
                .await
                .or_internal()?;
            issues.retain(|i| {
                let names: Vec<&str> = by_issue.get(&i.issue_id).map_or(Vec::new(), |ls| {
                    ls.iter().map(|l| l.name.as_str()).collect()
                });
                labels
                    .iter()
                    .all(|l| names.iter().any(|n| n.eq_ignore_ascii_case(l)))
            });
        }
        if let Some(author) = &filter.created_by {
            issues.retain(|i| i.author.eq_ignore_ascii_case(author));
        }
        if let Some(assignee) = &filter.assignee {
            let by_issue = db::assignees_by_issue(&state.db, self.repo.id)
                .await
                .or_internal()?;
            issues.retain(|i| {
                by_issue
                    .get(&i.issue_id)
                    .is_some_and(|names| names.iter().any(|n| n.eq_ignore_ascii_case(assignee)))
            });
        }
        if let Some(mentioned) = &filter.mentioned {
            let needle = format!("@{}", mentioned.to_lowercase());
            issues.retain(|i| i.body.to_lowercase().contains(&needle));
        }
        sort_by(
            &mut issues,
            order_by.as_ref(),
            |i| i.created,
            |i| i.number,
            |i| i.comment_count,
        );
        let nodes = issues
            .into_iter()
            .map(|issue| IssueNode {
                issue,
                repo: self.clone(),
            })
            .collect();
        Ok(IssueConnection::page(nodes, first, last, after.as_deref()))
    }

    async fn issue(&self, ctx: &Context<'_>, number: i64) -> Result<Option<IssueNode>> {
        let issue = db::issue(&api(ctx).state.db, self.repo.id, number)
            .await
            .or_internal()?;
        match issue {
            Some(issue) => Ok(Some(IssueNode {
                issue,
                repo: self.clone(),
            })),
            None => Err(not_found(&format!(
                "Could not resolve to an Issue with the number of {number}."
            ))),
        }
    }

    async fn issue_or_pull_request(
        &self,
        ctx: &Context<'_>,
        number: i64,
    ) -> Result<Option<IssueOrPullRequest>> {
        let db = &api(ctx).state.db;
        if let Some(issue) = db::issue(db, self.repo.id, number).await.or_internal()? {
            return Ok(Some(IssueOrPullRequest::Issue(IssueNode {
                issue,
                repo: self.clone(),
            })));
        }
        if let Some(pull) = db::pull(db, self.repo.id, number).await.or_internal()? {
            return Ok(Some(IssueOrPullRequest::PullRequest(PullRequestNode {
                pull,
                repo: self.clone(),
            })));
        }
        Err(not_found(&format!(
            "Could not resolve to an issue or pull request with the number of {number}."
        )))
    }

    async fn labels(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
        query: Option<String>,
        order_by: Option<LabelOrder>,
    ) -> Result<LabelConnection> {
        let state = &api(ctx).state;
        let url = self.web_url(state);
        let mut labels: Vec<Label> = db::labels(&state.db, self.repo.id)
            .await
            .or_internal()?
            .iter()
            .map(|l| label(&url, l))
            .collect();
        if let Some(query) = query.map(|q| q.to_lowercase()) {
            labels.retain(|l| l.name.to_lowercase().contains(&query));
        }
        // Labels come sorted by name; creation order isn't tracked, so ids stand in.
        if order_by
            .as_ref()
            .is_some_and(|o| o.field == LabelOrderField::CreatedAt)
        {
            labels.sort_by_key(|l| parse_id(&l.id, "LA").unwrap_or(0));
        }
        if order_by
            .as_ref()
            .is_some_and(|o| o.direction == OrderDirection::Desc)
        {
            labels.reverse();
        }
        Ok(LabelConnection::page(labels, first, last, after.as_deref()))
    }

    /// People who can be assigned or asked for review: the owner and collaborators.
    async fn assignable_users(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
    ) -> Result<UserConnection> {
        let users = people(api(ctx), &self.repo, query).await?;
        Ok(UserConnection::page(users, first, None, after.as_deref()))
    }

    async fn mentionable_users(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
        query: Option<String>,
    ) -> Result<UserConnection> {
        let users = people(api(ctx), &self.repo, query).await?;
        Ok(UserConnection::page(users, first, None, after.as_deref()))
    }
}

async fn people(api: &ApiContext, repo: &db::Repo, query: Option<String>) -> Result<Vec<User>> {
    let mut users = vec![user(&api.state, repo.owner_id, &repo.owner_name)];
    for (id, name, _) in db::collaborators(&api.state.db, repo.id)
        .await
        .or_internal()?
    {
        users.push(user(&api.state, id, &name));
    }
    if let Some(query) = query.map(|q| q.to_lowercase()) {
        users.retain(|u| u.login.to_lowercase().contains(&query));
    }
    Ok(users)
}

/// Sort issues or pull requests by `orderBy` (newest first by default).
fn sort_by<T>(
    items: &mut [T],
    order: Option<&IssueOrder>,
    created: impl Fn(&T) -> i64,
    number: impl Fn(&T) -> i64,
    comments: impl Fn(&T) -> i64,
) {
    let field = order.map_or(IssueOrderField::CreatedAt, |o| o.field);
    items.sort_by_key(|item| match field {
        IssueOrderField::Comments => (comments(item), number(item)),
        // Numbers increase with creation; updated time isn't tracked separately yet.
        IssueOrderField::CreatedAt | IssueOrderField::UpdatedAt => (created(item), number(item)),
    });
    if order.is_none_or(|o| o.direction == OrderDirection::Desc) {
        items.reverse();
    }
}

// ---- Comments and reviews ----

#[derive(Clone)]
pub struct IssueComment {
    comment: db::Comment,
    url: String,
    viewer_did_author: bool,
    author: User,
    association: CommentAuthorAssociation,
}

#[Object]
impl IssueComment {
    async fn id(&self) -> ID {
        ID(format!("IC_{}", self.comment.id))
    }

    async fn database_id(&self) -> i64 {
        self.comment.id
    }

    async fn author(&self) -> Option<User> {
        Some(self.author.clone())
    }

    async fn author_association(&self) -> CommentAuthorAssociation {
        self.association
    }

    async fn body(&self) -> &str {
        &self.comment.body
    }

    async fn created_at(&self) -> String {
        iso(self.comment.created)
    }

    async fn includes_created_edit(&self) -> bool {
        false
    }

    async fn is_minimized(&self) -> bool {
        false
    }

    async fn minimized_reason(&self) -> Option<String> {
        None
    }

    async fn reaction_groups(&self) -> Vec<ReactionGroup> {
        Vec::new()
    }

    async fn url(&self) -> &str {
        &self.url
    }

    async fn viewer_did_author(&self) -> bool {
        self.viewer_did_author
    }
}

connection!(
    IssueCommentConnection,
    IssueCommentConnectionEdge,
    IssueComment
);

#[derive(SimpleObject)]
pub struct IssueCommentEdge {
    node: IssueComment,
    cursor: String,
}

async fn comment_connection(
    api: &ApiContext,
    repo: &RepositoryNode,
    issue_id: i64,
    page_url: &str,
    (first, last, after): (Option<i32>, Option<i32>, Option<String>),
) -> Result<IssueCommentConnection> {
    let comments = db::comments(&api.state.db, issue_id).await.or_internal()?;
    let users = user_ids(api).await?;
    let collaborators = collaborator_names(api, &repo.repo).await?;
    let nodes = comments
        .into_iter()
        // Comments on diff lines are review comments, not part of the conversation.
        .filter(|c| c.path.is_none())
        .map(|c| issue_comment(api, &users, &collaborators, &repo.repo, page_url, c))
        .collect();
    Ok(IssueCommentConnection::page(
        nodes,
        first,
        last,
        after.as_deref(),
    ))
}

fn issue_comment(
    api: &ApiContext,
    users: &HashMap<String, i64>,
    collaborators: &[String],
    repo: &db::Repo,
    page_url: &str,
    c: db::Comment,
) -> IssueComment {
    let id = users.get(&c.author).copied().unwrap_or(0);
    IssueComment {
        url: format!("{page_url}#comment-{}", c.id),
        viewer_did_author: api.viewer.as_ref().is_some_and(|v| v.username == c.author),
        author: user(&api.state, id, &c.author),
        association: association(repo, collaborators, &c.author),
        comment: c,
    }
}

fn association(repo: &db::Repo, collaborators: &[String], login: &str) -> CommentAuthorAssociation {
    if login == repo.owner_name {
        CommentAuthorAssociation::Owner
    } else if collaborators.iter().any(|c| c == login) {
        CommentAuthorAssociation::Collaborator
    } else {
        CommentAuthorAssociation::NoAssociation
    }
}

async fn user_ids(api: &ApiContext) -> Result<HashMap<String, i64>> {
    Ok(db::all_users(&api.state.db)
        .await
        .or_internal()?
        .into_iter()
        .map(|(id, name)| (name, id))
        .collect())
}

async fn collaborator_names(api: &ApiContext, repo: &db::Repo) -> Result<Vec<String>> {
    Ok(db::collaborators(&api.state.db, repo.id)
        .await
        .or_internal()?
        .into_iter()
        .map(|(_, name, _)| name)
        .collect())
}

#[derive(Clone)]
pub struct Review {
    review: db::Review,
    author: User,
    association: CommentAuthorAssociation,
}

#[Object(name = "PullRequestReview")]
impl Review {
    async fn id(&self) -> ID {
        ID(format!("PRR_{}", self.review.id))
    }

    async fn database_id(&self) -> i64 {
        self.review.id
    }

    async fn author(&self) -> Option<User> {
        Some(self.author.clone())
    }

    async fn author_association(&self) -> CommentAuthorAssociation {
        self.association
    }

    async fn body(&self) -> &str {
        &self.review.body
    }

    async fn state(&self) -> PullRequestReviewState {
        match self.review.state.as_str() {
            "approved" => PullRequestReviewState::Approved,
            "changes_requested" => PullRequestReviewState::ChangesRequested,
            _ => PullRequestReviewState::Commented,
        }
    }

    async fn submitted_at(&self) -> String {
        iso(self.review.created)
    }

    async fn commit(&self) -> Option<CommitOid> {
        Some(CommitOid {
            oid: self.review.commit_sha.clone(),
        })
    }

    async fn reaction_groups(&self) -> Vec<ReactionGroup> {
        Vec::new()
    }
}

/// Just a commit's id, where that's all there is.
#[derive(SimpleObject)]
#[graphql(name = "CommitRef")]
pub struct CommitOid {
    oid: String,
}

connection!(PullRequestReviewConnection, PullRequestReviewEdge, Review);

// ---- Pull requests ----

#[derive(Clone)]
pub struct PullRequestNode {
    pull: db::Pull,
    repo: RepositoryNode,
}

fn pull_state(pull: &db::Pull) -> PullRequestState {
    if pull.is_merged() {
        PullRequestState::Merged
    } else if pull.is_open() {
        PullRequestState::Open
    } else {
        PullRequestState::Closed
    }
}

impl PullRequestNode {
    fn page_url(&self, state: &AppState) -> String {
        format!("{}/pull/{}", self.repo.web_url(state), self.pull.number)
    }

    async fn tips(&self, state: &AppState) -> Result<actions::Tips> {
        actions::pull_tips(state, &self.repo.git(state), &self.pull)
            .await
            .or_internal()
    }

    async fn diff(&self, state: &AppState) -> Result<Vec<forge_git::diff::FileDiff>> {
        let tips = self.tips(state).await?;
        self.repo
            .git(state)
            .compare_diff(&tips.base_sha, &tips.head_sha)
            .await
            .or_internal()
    }
}

#[derive(SimpleObject, Clone)]
pub struct PullRequestChangedFile {
    path: String,
    additions: i64,
    deletions: i64,
    change_type: String,
}

connection!(
    PullRequestChangedFileConnection,
    PullRequestChangedFileEdge,
    PullRequestChangedFile
);

#[Object(name = "PullRequest")]
impl PullRequestNode {
    async fn id(&self) -> ID {
        ID(format!("PR_{}", self.pull.issue_id))
    }

    async fn database_id(&self) -> i64 {
        self.pull.issue_id
    }

    async fn number(&self) -> i64 {
        self.pull.number
    }

    async fn title(&self) -> &str {
        &self.pull.title
    }

    async fn body(&self) -> &str {
        &self.pull.body
    }

    async fn url(&self, ctx: &Context<'_>) -> String {
        self.page_url(&api(ctx).state)
    }

    async fn state(&self) -> PullRequestState {
        pull_state(&self.pull)
    }

    async fn closed(&self) -> bool {
        !self.pull.is_open()
    }

    async fn merged(&self) -> bool {
        self.pull.is_merged()
    }

    async fn merged_at(&self) -> Option<String> {
        self.pull.merged.map(iso)
    }

    async fn merged_by(&self, ctx: &Context<'_>) -> Result<Option<User>> {
        let Some(login) = &self.pull.merged_by else {
            return Ok(None);
        };
        let ids = user_ids(api(ctx)).await?;
        Ok(Some(user(
            &api(ctx).state,
            ids.get(login).copied().unwrap_or(0),
            login,
        )))
    }

    async fn created_at(&self) -> String {
        iso(self.pull.created)
    }

    async fn updated_at(&self) -> String {
        iso(self.pull.created)
    }

    async fn author(&self, ctx: &Context<'_>) -> Option<User> {
        Some(user(
            &api(ctx).state,
            self.pull.author_id,
            &self.pull.author,
        ))
    }

    async fn author_association(&self, ctx: &Context<'_>) -> Result<CommentAuthorAssociation> {
        let collaborators = collaborator_names(api(ctx), &self.repo.repo).await?;
        Ok(association(
            &self.repo.repo,
            &collaborators,
            &self.pull.author,
        ))
    }

    async fn is_draft(&self) -> bool {
        false
    }

    async fn maintainer_can_modify(&self) -> bool {
        false
    }

    async fn locked(&self) -> bool {
        false
    }

    async fn mergeable(&self, ctx: &Context<'_>) -> Result<MergeableState> {
        let state = &api(ctx).state;
        if !self.pull.is_open() {
            return Ok(MergeableState::Unknown);
        }
        let tips = self.tips(state).await?;
        if !tips.head_exists || !tips.base_exists {
            return Ok(MergeableState::Unknown);
        }
        Ok(
            match self
                .repo
                .git(state)
                .mergeability(&tips.base_sha, &tips.head_sha)
                .await
                .or_internal()?
            {
                Mergeability::Clean | Mergeability::UpToDate => MergeableState::Mergeable,
                Mergeability::Conflicts(_) => MergeableState::Conflicting,
            },
        )
    }

    async fn additions(&self, ctx: &Context<'_>) -> Result<i64> {
        Ok(self
            .diff(&api(ctx).state)
            .await?
            .iter()
            .map(|f| f.additions as i64)
            .sum())
    }

    async fn deletions(&self, ctx: &Context<'_>) -> Result<i64> {
        Ok(self
            .diff(&api(ctx).state)
            .await?
            .iter()
            .map(|f| f.deletions as i64)
            .sum())
    }

    async fn changed_files(&self, ctx: &Context<'_>) -> Result<i64> {
        Ok(self.diff(&api(ctx).state).await?.len() as i64)
    }

    async fn files(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<PullRequestChangedFileConnection> {
        let files = self
            .diff(&api(ctx).state)
            .await?
            .into_iter()
            .map(|f| PullRequestChangedFile {
                path: f.path().to_owned(),
                additions: f.additions as i64,
                deletions: f.deletions as i64,
                change_type: match f.status {
                    forge_git::diff::FileStatus::Added => "ADDED",
                    forge_git::diff::FileStatus::Deleted => "DELETED",
                    forge_git::diff::FileStatus::Renamed => "RENAMED",
                    forge_git::diff::FileStatus::Modified => "MODIFIED",
                }
                .into(),
            })
            .collect();
        Ok(PullRequestChangedFileConnection::page(
            files,
            first,
            None,
            after.as_deref(),
        ))
    }

    async fn commits(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
    ) -> Result<PullRequestCommitConnection> {
        let state = &api(ctx).state;
        let tips = self.tips(state).await?;
        let commits = self
            .repo
            .git(state)
            .compare_log(&tips.base_sha, &tips.head_sha, 250)
            .await
            .or_internal()?;
        let base = self.repo.web_url(state);
        let nodes = commits
            .into_iter()
            .map(|commit| PullRequestCommit {
                commit: Commit {
                    url: format!("{base}/commit/{}", commit.id),
                    commit,
                    repo: self.repo.repo.clone(),
                },
            })
            .collect();
        Ok(PullRequestCommitConnection::page(
            nodes,
            first,
            last,
            after.as_deref(),
        ))
    }

    async fn base_ref_name(&self) -> &str {
        &self.pull.base_branch
    }

    async fn head_ref_name(&self) -> &str {
        &self.pull.head_branch
    }

    async fn base_ref_oid(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(self.tips(&api(ctx).state).await?.base_sha)
    }

    async fn head_ref_oid(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(self.tips(&api(ctx).state).await?.head_sha)
    }

    async fn base_repository(&self) -> Option<RepositoryNode> {
        Some(self.repo.clone())
    }

    async fn head_repository(&self) -> Option<RepositoryNode> {
        Some(self.repo.clone())
    }

    async fn head_repository_owner(&self, ctx: &Context<'_>) -> Option<User> {
        Some(user(
            &api(ctx).state,
            self.repo.repo.owner_id,
            &self.repo.repo.owner_name,
        ))
    }

    async fn is_cross_repository(&self) -> bool {
        false
    }

    async fn review_requests(&self, first: Option<i32>) -> ReviewRequestConnection {
        ReviewRequestConnection::page(Vec::new(), first, None, None)
    }

    async fn reviews(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
    ) -> Result<PullRequestReviewConnection> {
        let api = api(ctx);
        let ids = user_ids(api).await?;
        let collaborators = collaborator_names(api, &self.repo.repo).await?;
        let reviews = db::reviews(&api.state.db, self.pull.issue_id)
            .await
            .or_internal()?
            .into_iter()
            .map(|review| Review {
                author: user(
                    &api.state,
                    ids.get(&review.author).copied().unwrap_or(0),
                    &review.author,
                ),
                association: association(&self.repo.repo, &collaborators, &review.author),
                review,
            })
            .collect();
        Ok(PullRequestReviewConnection::page(
            reviews,
            first,
            last,
            after.as_deref(),
        ))
    }

    async fn latest_reviews(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<PullRequestReviewConnection> {
        let api = api(ctx);
        let ids = user_ids(api).await?;
        let collaborators = collaborator_names(api, &self.repo.repo).await?;
        let mut latest: Vec<db::Review> = Vec::new();
        for review in db::reviews(&api.state.db, self.pull.issue_id)
            .await
            .or_internal()?
        {
            latest.retain(|r| r.author != review.author);
            latest.push(review);
        }
        let reviews = latest
            .into_iter()
            .map(|review| Review {
                author: user(
                    &api.state,
                    ids.get(&review.author).copied().unwrap_or(0),
                    &review.author,
                ),
                association: association(&self.repo.repo, &collaborators, &review.author),
                review,
            })
            .collect();
        Ok(PullRequestReviewConnection::page(
            reviews, first, None, None,
        ))
    }

    async fn review_decision(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Option<PullRequestReviewDecision>> {
        let mut latest: HashMap<String, String> = HashMap::new();
        for review in db::reviews(&api(ctx).state.db, self.pull.issue_id)
            .await
            .or_internal()?
        {
            if review.state != "commented" {
                latest.insert(review.author, review.state);
            }
        }
        Ok(if latest.values().any(|s| s == "changes_requested") {
            Some(PullRequestReviewDecision::ChangesRequested)
        } else if latest.values().any(|s| s == "approved") {
            Some(PullRequestReviewDecision::Approved)
        } else {
            None
        })
    }

    async fn assignees(&self, ctx: &Context<'_>, first: Option<i32>) -> Result<UserConnection> {
        assignees(api(ctx), self.pull.issue_id, first).await
    }

    async fn labels(&self, ctx: &Context<'_>, first: Option<i32>) -> Result<LabelConnection> {
        labels_of(api(ctx), &self.repo, self.pull.issue_id, first).await
    }

    async fn milestone(&self) -> Option<Milestone> {
        None
    }

    async fn comments(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
    ) -> Result<IssueCommentConnection> {
        let api = api(ctx);
        let url = self.page_url(&api.state);
        comment_connection(
            api,
            &self.repo,
            self.pull.issue_id,
            &url,
            (first, last, after),
        )
        .await
    }

    async fn reaction_groups(&self) -> Vec<ReactionGroup> {
        Vec::new()
    }

    async fn auto_merge_request(&self) -> Option<AutoMergeRequest> {
        None
    }

    async fn merge_state_status(&self, ctx: &Context<'_>) -> Result<MergeStateStatus> {
        let state = &api(ctx).state;
        if !self.pull.is_open() {
            return Ok(MergeStateStatus::Unknown);
        }
        let tips = self.tips(state).await?;
        if !tips.head_exists || !tips.base_exists {
            return Ok(MergeStateStatus::Unknown);
        }
        let mergeability = self
            .repo
            .git(state)
            .mergeability(&tips.base_sha, &tips.head_sha)
            .await
            .or_internal()?;
        if let Mergeability::Conflicts(_) = mergeability {
            return Ok(MergeStateStatus::Dirty);
        }
        let blocked = actions::merge_blocker(state, &self.repo.repo, &self.pull, &tips.head_sha)
            .await
            .or_internal()?;
        Ok(if blocked.is_some() {
            MergeStateStatus::Blocked
        } else {
            MergeStateStatus::Clean
        })
    }

    async fn base_ref(&self) -> Option<Ref> {
        Some(Ref {
            name: self.pull.base_branch.clone(),
            prefix: "refs/heads/".into(),
            repo_id: self.repo.repo.id,
        })
    }

    async fn head_ref(&self, ctx: &Context<'_>) -> Result<Option<Ref>> {
        let exists = self.tips(&api(ctx).state).await?.head_exists;
        Ok(exists.then(|| Ref {
            name: self.pull.head_branch.clone(),
            prefix: "refs/heads/".into(),
            repo_id: self.repo.repo.id,
        }))
    }

    async fn project_items(
        &self,
        first: Option<i32>,
        after: Option<String>,
    ) -> ProjectV2ItemConnection {
        ProjectV2ItemConnection::page(Vec::new(), first, None, after.as_deref())
    }

    async fn closing_issues_references(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
    ) -> Result<IssueConnection> {
        let db = &api(ctx).state.db;
        let mut issues = Vec::new();
        for number in
            web::render::closing_references(&format!("{}\n{}", self.pull.title, self.pull.body))
        {
            if let Some(issue) = db::issue(db, self.repo.repo.id, number)
                .await
                .or_internal()?
            {
                issues.push(IssueNode {
                    issue,
                    repo: self.repo.clone(),
                });
            }
        }
        Ok(IssueConnection::page(issues, first, None, None))
    }
}

async fn assignees(api: &ApiContext, issue_id: i64, first: Option<i32>) -> Result<UserConnection> {
    let users = db::assignees(&api.state.db, issue_id)
        .await
        .or_internal()?
        .into_iter()
        .map(|(id, name)| user(&api.state, id, &name))
        .collect();
    Ok(UserConnection::page(users, first, None, None))
}

async fn labels_of(
    api: &ApiContext,
    repo: &RepositoryNode,
    issue_id: i64,
    first: Option<i32>,
) -> Result<LabelConnection> {
    let url = repo.web_url(&api.state);
    let labels = db::labels_by_issue(&api.state.db, repo.repo.id)
        .await
        .or_internal()?
        .remove(&issue_id)
        .unwrap_or_default()
        .iter()
        .map(|l| label(&url, l))
        .collect();
    Ok(LabelConnection::page(labels, first, None, None))
}

connection!(PullRequestConnection, PullRequestEdge, PullRequestNode);

// ---- Issues ----

#[derive(Clone)]
pub struct IssueNode {
    issue: db::Issue,
    repo: RepositoryNode,
}

impl IssueNode {
    fn page_url(&self, state: &AppState) -> String {
        format!("{}/issues/{}", self.repo.web_url(state), self.issue.number)
    }
}

#[Object(name = "Issue")]
impl IssueNode {
    async fn id(&self) -> ID {
        ID(format!("I_{}", self.issue.issue_id))
    }

    async fn database_id(&self) -> i64 {
        self.issue.issue_id
    }

    async fn number(&self) -> i64 {
        self.issue.number
    }

    async fn title(&self) -> &str {
        &self.issue.title
    }

    async fn body(&self) -> &str {
        &self.issue.body
    }

    async fn url(&self, ctx: &Context<'_>) -> String {
        self.page_url(&api(ctx).state)
    }

    async fn state(&self) -> IssueState {
        if self.issue.is_open() {
            IssueState::Open
        } else {
            IssueState::Closed
        }
    }

    async fn state_reason(&self) -> Option<IssueStateReason> {
        (!self.issue.is_open()).then_some(IssueStateReason::Completed)
    }

    async fn closed(&self) -> bool {
        !self.issue.is_open()
    }

    async fn created_at(&self) -> String {
        iso(self.issue.created)
    }

    async fn updated_at(&self) -> String {
        iso(self.issue.created)
    }

    async fn author(&self, ctx: &Context<'_>) -> Option<User> {
        Some(user(
            &api(ctx).state,
            self.issue.author_id,
            &self.issue.author,
        ))
    }

    async fn assignees(&self, ctx: &Context<'_>, first: Option<i32>) -> Result<UserConnection> {
        assignees(api(ctx), self.issue.issue_id, first).await
    }

    async fn labels(&self, ctx: &Context<'_>, first: Option<i32>) -> Result<LabelConnection> {
        labels_of(api(ctx), &self.repo, self.issue.issue_id, first).await
    }

    async fn milestone(&self) -> Option<Milestone> {
        None
    }

    async fn comments(
        &self,
        ctx: &Context<'_>,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
    ) -> Result<IssueCommentConnection> {
        let api = api(ctx);
        let url = self.page_url(&api.state);
        comment_connection(
            api,
            &self.repo,
            self.issue.issue_id,
            &url,
            (first, last, after),
        )
        .await
    }

    async fn reaction_groups(&self) -> Vec<ReactionGroup> {
        Vec::new()
    }

    async fn repository(&self) -> RepositoryNode {
        self.repo.clone()
    }

    async fn project_items(
        &self,
        first: Option<i32>,
        after: Option<String>,
    ) -> ProjectV2ItemConnection {
        ProjectV2ItemConnection::page(Vec::new(), first, None, after.as_deref())
    }

    async fn issue_type(&self) -> Option<IssueType> {
        None
    }

    async fn parent(&self) -> Option<IssueNode> {
        None
    }

    async fn sub_issues(&self, first: Option<i32>, after: Option<String>) -> IssueConnection {
        IssueConnection::page(Vec::new(), first, None, after.as_deref())
    }

    async fn sub_issues_summary(&self) -> SubIssuesSummary {
        SubIssuesSummary {
            total: 0,
            completed: 0,
            percent_completed: 0,
        }
    }
}

connection!(IssueConnection, IssueEdge, IssueNode);

// ---- Query ----

#[derive(async_graphql::Interface)]
#[graphql(field(name = "id", ty = "ID"))]
pub enum Node {
    Repository(RepositoryNode),
    PullRequest(PullRequestNode),
    Issue(IssueNode),
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum SearchType {
    Issue,
    Repository,
    User,
    Discussion,
}

#[derive(Union, Clone)]
pub enum SearchResultItem {
    Issue(IssueNode),
    PullRequest(PullRequestNode),
}

connection!(
    SearchResultItemConnectionInner,
    SearchResultItemEdge,
    SearchResultItem
);

pub struct SearchResultItemConnection {
    inner: SearchResultItemConnectionInner,
}

#[Object]
impl SearchResultItemConnection {
    async fn issue_count(&self) -> i64 {
        self.inner.total_count
    }

    async fn nodes(&self) -> &Vec<SearchResultItem> {
        &self.inner.nodes
    }

    async fn edges(&self) -> &Vec<SearchResultItemEdge> {
        &self.inner.edges
    }

    async fn page_info(&self) -> &PageInfo {
        &self.inner.page_info
    }
}

/// Issue search, enough for `gh pr status` / `gh search`: `repo:`, `is:pr`/`is:issue`,
/// `is:open`/`is:closed`/`is:merged`, `state:`, `author:`, `assignee:`, `involves:`,
/// `review-requested:` and words in titles. `@me` means the viewer.
async fn search(api: &ApiContext, query: &str) -> Result<Vec<SearchResultItem>> {
    let me = api
        .viewer
        .as_ref()
        .map(|v| v.username.clone())
        .unwrap_or_default();
    let resolve = |v: &str| if v == "@me" { me.clone() } else { v.to_owned() };
    let (mut repos, mut kind, mut state, mut author, mut assignee, mut involves) =
        (Vec::new(), None::<&str>, None::<String>, None, None, None);
    let mut words = Vec::new();
    let mut nothing = false;
    for term in query.split_whitespace() {
        match term.split_once(':') {
            Some(("repo", v)) => repos.push(v.to_owned()),
            Some(("is" | "type", "pr" | "pull-request")) => kind = Some("pr"),
            Some(("is" | "type", "issue")) => kind = Some("issue"),
            Some(("is" | "state", v @ ("open" | "closed" | "merged"))) => {
                state = Some(v.to_owned())
            }
            Some(("author", v)) => author = Some(resolve(v)),
            Some(("assignee", v)) => assignee = Some(resolve(v)),
            Some(("involves" | "mentions" | "commenter", v)) => involves = Some(resolve(v)),
            // No review requests in omaforge yet.
            Some(("review-requested" | "team-review-requested", _)) => nothing = true,
            Some(("sort" | "archived" | "draft" | "in", _)) => {}
            _ => words.push(term.to_lowercase()),
        }
    }
    if nothing {
        return Ok(Vec::new());
    }
    let db = &api.state.db;
    let mut nodes = Vec::new();
    for full_name in repos {
        let Some((owner, name)) = full_name.split_once('/') else {
            continue;
        };
        let Ok(Some(repo)) = repository(api, owner, name).await else {
            continue;
        };
        let assignees = db::assignees_by_issue(db, repo.repo.id)
            .await
            .or_internal()?;
        let matches = |issue_id: i64, title: &str, body: &str, who: &str| {
            author.as_ref().is_none_or(|a| a.eq_ignore_ascii_case(who))
                && assignee.as_ref().is_none_or(|a| {
                    assignees
                        .get(&issue_id)
                        .is_some_and(|ns| ns.iter().any(|n| n.eq_ignore_ascii_case(a)))
                })
                && involves.as_ref().is_none_or(|a| {
                    a.eq_ignore_ascii_case(who)
                        || body
                            .to_lowercase()
                            .contains(&format!("@{}", a.to_lowercase()))
                        || assignees
                            .get(&issue_id)
                            .is_some_and(|ns| ns.iter().any(|n| n.eq_ignore_ascii_case(a)))
                })
                && words
                    .iter()
                    .all(|w| title.to_lowercase().contains(w) || body.to_lowercase().contains(w))
        };
        if kind != Some("issue") {
            for db_state in ["open", "closed"] {
                for pull in db::pulls(db, repo.repo.id, db_state).await.or_internal()? {
                    let s = match pull_state(&pull) {
                        PullRequestState::Open => "open",
                        PullRequestState::Merged => "merged",
                        PullRequestState::Closed => "closed",
                    };
                    let state_ok = state
                        .as_deref()
                        .is_none_or(|w| w == s || (w == "closed" && s == "merged"));
                    if state_ok && matches(pull.issue_id, &pull.title, &pull.body, &pull.author) {
                        nodes.push((
                            pull.created,
                            SearchResultItem::PullRequest(PullRequestNode {
                                pull,
                                repo: repo.clone(),
                            }),
                        ));
                    }
                }
            }
        }
        if kind != Some("pr") && state.as_deref() != Some("merged") {
            for db_state in ["open", "closed"] {
                if state.as_deref().is_some_and(|w| w != db_state) {
                    continue;
                }
                for issue in db::issues(db, repo.repo.id, db_state, None, None)
                    .await
                    .or_internal()?
                {
                    if matches(issue.issue_id, &issue.title, &issue.body, &issue.author) {
                        nodes.push((
                            issue.created,
                            SearchResultItem::Issue(IssueNode {
                                issue,
                                repo: repo.clone(),
                            }),
                        ));
                    }
                }
            }
        }
    }
    nodes.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
    Ok(nodes.into_iter().map(|(_, node)| node).collect())
}

pub struct Query;

#[Object]
impl Query {
    async fn repository(
        &self,
        ctx: &Context<'_>,
        owner: String,
        name: String,
    ) -> Result<Option<RepositoryNode>> {
        repository(api(ctx), &owner, &name).await
    }

    async fn node(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Node>> {
        let api = api(ctx);
        Ok(Some(if id.starts_with("PR_") {
            Node::PullRequest(pull_by_id(api, &id).await?)
        } else if id.starts_with("I_") {
            Node::Issue(issue_by_id(api, &id).await?)
        } else if id.starts_with("R_") {
            Node::Repository(repository_by_id(api, &id).await?)
        } else {
            return Err(not_found(&format!(
                "Could not resolve to a node with the global id of '{}'",
                id.as_str()
            )));
        }))
    }

    async fn search(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(name = "type")] kind: SearchType,
        first: Option<i32>,
        last: Option<i32>,
        after: Option<String>,
    ) -> Result<SearchResultItemConnection> {
        let items = if kind == SearchType::Issue {
            search(api(ctx), &query).await?
        } else {
            Vec::new()
        };
        Ok(SearchResultItemConnection {
            inner: SearchResultItemConnectionInner::page(items, first, last, after.as_deref()),
        })
    }

    async fn viewer(&self, ctx: &Context<'_>) -> Result<User> {
        let api = api(ctx);
        let viewer = api
            .viewer
            .as_ref()
            .ok_or_else(|| forbidden("This endpoint requires you to be authenticated."))?;
        Ok(user(&api.state, viewer.id, &viewer.username))
    }

    async fn user(&self, ctx: &Context<'_>, login: String) -> Result<Option<User>> {
        let api = api(ctx);
        let found = db::user_by_username(&api.state.db, &login)
            .await
            .or_internal()?;
        Ok(found.map(|u| user(&api.state, u.id, &u.username)))
    }
}

// ---- Mutations ----

pub struct Mutation;

/// The signed-in user, who must have at least `needed` access to `repo`.
fn require<'a>(api: &'a ApiContext, repo: &RepositoryNode, needed: Access) -> Result<&'a db::User> {
    let viewer = api
        .viewer
        .as_ref()
        .ok_or_else(|| forbidden("This endpoint requires you to be authenticated."))?;
    if repo.access < needed {
        return Err(forbidden(&format!(
            "{} does not have the correct permissions to execute this action",
            viewer.username
        )));
    }
    Ok(viewer)
}

/// A pull request from its global id, with its repository.
async fn pull_by_id(api: &ApiContext, id: &ID) -> Result<PullRequestNode> {
    let issue_id = parse_id(id, "PR")?;
    let (repo_id, number) = db::issue_location(&api.state.db, issue_id)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to a PullRequest"))?;
    let repo = repository_by_id(api, &ID(format!("R_{repo_id}"))).await?;
    let pull = db::pull(&api.state.db, repo_id, number)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to a PullRequest"))?;
    Ok(PullRequestNode { pull, repo })
}

async fn issue_by_id(api: &ApiContext, id: &ID) -> Result<IssueNode> {
    let issue_id = parse_id(id, "I")?;
    let (repo_id, number) = db::issue_location(&api.state.db, issue_id)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to an Issue"))?;
    let repo = repository_by_id(api, &ID(format!("R_{repo_id}"))).await?;
    let issue = db::issue(&api.state.db, repo_id, number)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to an Issue"))?;
    Ok(IssueNode { issue, repo })
}

async fn reload_pull(api: &ApiContext, node: &PullRequestNode) -> Result<PullRequestNode> {
    let pull = db::pull(&api.state.db, node.repo.repo.id, node.pull.number)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to a PullRequest"))?;
    Ok(PullRequestNode {
        pull,
        repo: node.repo.clone(),
    })
}

async fn reload_issue(api: &ApiContext, node: &IssueNode) -> Result<IssueNode> {
    let issue = db::issue(&api.state.db, node.repo.repo.id, node.issue.number)
        .await
        .or_internal()?
        .ok_or_else(|| not_found("Could not resolve to an Issue"))?;
    Ok(IssueNode {
        issue,
        repo: node.repo.clone(),
    })
}

#[derive(InputObject)]
pub struct CreatePullRequestInput {
    repository_id: ID,
    base_ref_name: String,
    head_ref_name: String,
    head_repository_id: Option<ID>,
    title: String,
    body: Option<String>,
    draft: Option<bool>,
    maintainer_can_modify: Option<bool>,
    client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct CreatePullRequestPayload {
    pull_request: Option<PullRequestNode>,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct MergePullRequestInput {
    pull_request_id: ID,
    merge_method: Option<PullRequestMergeMethod>,
    commit_headline: Option<String>,
    commit_body: Option<String>,
    expected_head_oid: Option<String>,
    author_email: Option<String>,
    client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct MergePullRequestPayload {
    pull_request: Option<PullRequestNode>,
    client_mutation_id: Option<String>,
}

pub struct PullRequestIdInput {
    pull_request_id: ID,
    client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct PullRequestPayload {
    pull_request: Option<PullRequestNode>,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct CreateIssueInput {
    repository_id: ID,
    title: String,
    body: Option<String>,
    assignee_ids: Option<Vec<ID>>,
    label_ids: Option<Vec<ID>>,
    milestone_id: Option<ID>,
    project_ids: Option<Vec<ID>>,
    issue_template: Option<String>,
    client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct IssuePayload {
    issue: Option<IssueNode>,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct CloseIssueInput {
    issue_id: ID,
    state_reason: Option<IssueClosedStateReason>,
    duplicate_issue_id: Option<ID>,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct ReopenIssueInput {
    issue_id: ID,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct AddCommentInput {
    subject_id: ID,
    body: String,
    client_mutation_id: Option<String>,
}

#[derive(Union)]
pub enum CommentSubject {
    Issue(IssueNode),
    PullRequest(PullRequestNode),
}

#[derive(SimpleObject)]
pub struct AddCommentPayload {
    comment_edge: Option<IssueCommentEdge>,
    subject: Option<CommentSubject>,
    client_mutation_id: Option<String>,
}

#[derive(Enum, Copy, Clone, Eq, PartialEq)]
pub enum PullRequestReviewEvent {
    Approve,
    RequestChanges,
    Comment,
    Dismiss,
}

#[derive(InputObject)]
pub struct AddPullRequestReviewInput {
    pull_request_id: ID,
    event: Option<PullRequestReviewEvent>,
    body: Option<String>,
    #[graphql(name = "commitOID")]
    commit_oid: Option<String>,
    client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct AddPullRequestReviewPayload {
    pull_request_review: Option<Review>,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct UpdatePullRequestInput {
    pull_request_id: ID,
    title: Option<String>,
    body: Option<String>,
    base_ref_name: Option<String>,
    state: Option<PullRequestState>,
    maintainer_can_modify: Option<bool>,
    assignee_ids: Option<Vec<ID>>,
    label_ids: Option<Vec<ID>>,
    milestone_id: Option<ID>,
    project_ids: Option<Vec<ID>>,
    client_mutation_id: Option<String>,
}

#[derive(InputObject)]
pub struct UpdateIssueInput {
    id: ID,
    title: Option<String>,
    body: Option<String>,
    state: Option<IssueState>,
    assignee_ids: Option<Vec<ID>>,
    label_ids: Option<Vec<ID>>,
    milestone_id: Option<ID>,
    project_ids: Option<Vec<ID>>,
    client_mutation_id: Option<String>,
}

pub struct LabelableInput {
    labelable_id: ID,
    label_ids: Vec<ID>,
    client_mutation_id: Option<String>,
}

pub struct AssignableInput {
    assignable_id: ID,
    assignee_ids: Vec<ID>,
    client_mutation_id: Option<String>,
}

// `gh` declares variables with GitHub's input type names, so mutations that take the same
// fields still need their own, correctly named, input types.
macro_rules! labelable_input {
    ($name:ident) => {
        #[derive(InputObject)]
        pub struct $name {
            labelable_id: ID,
            label_ids: Vec<ID>,
            client_mutation_id: Option<String>,
        }

        impl From<$name> for LabelableInput {
            fn from(i: $name) -> Self {
                Self {
                    labelable_id: i.labelable_id,
                    label_ids: i.label_ids,
                    client_mutation_id: i.client_mutation_id,
                }
            }
        }
    };
}
labelable_input!(AddLabelsToLabelableInput);
labelable_input!(RemoveLabelsFromLabelableInput);

macro_rules! assignable_input {
    ($name:ident) => {
        #[derive(InputObject)]
        pub struct $name {
            assignable_id: ID,
            assignee_ids: Vec<ID>,
            client_mutation_id: Option<String>,
        }

        impl From<$name> for AssignableInput {
            fn from(i: $name) -> Self {
                Self {
                    assignable_id: i.assignable_id,
                    assignee_ids: i.assignee_ids,
                    client_mutation_id: i.client_mutation_id,
                }
            }
        }
    };
}
assignable_input!(AddAssigneesToAssignableInput);
assignable_input!(RemoveAssigneesFromAssignableInput);

macro_rules! pull_request_id_input {
    ($name:ident) => {
        #[derive(InputObject)]
        pub struct $name {
            pull_request_id: ID,
            client_mutation_id: Option<String>,
        }

        impl From<$name> for PullRequestIdInput {
            fn from(i: $name) -> Self {
                Self {
                    pull_request_id: i.pull_request_id,
                    client_mutation_id: i.client_mutation_id,
                }
            }
        }
    };
}
pull_request_id_input!(ClosePullRequestInput);
pull_request_id_input!(ReopenPullRequestInput);

#[derive(Union)]
pub enum Labelable {
    Issue(IssueNode),
    PullRequest(PullRequestNode),
}

#[derive(SimpleObject)]
pub struct LabelablePayload {
    labelable: Option<Labelable>,
    client_mutation_id: Option<String>,
}

#[derive(SimpleObject)]
pub struct AssignablePayload {
    assignable: Option<Labelable>,
    client_mutation_id: Option<String>,
}

/// An issue or pull request from its global id: `(issue_id, repo, author_id, node)`.
async fn issue_or_pull_by_id(
    api: &ApiContext,
    id: &ID,
) -> Result<(i64, RepositoryNode, i64, Labelable)> {
    if id.starts_with("PR_") {
        let node = pull_by_id(api, id).await?;
        Ok((
            node.pull.issue_id,
            node.repo.clone(),
            node.pull.author_id,
            Labelable::PullRequest(node),
        ))
    } else {
        let node = issue_by_id(api, id).await?;
        Ok((
            node.issue.issue_id,
            node.repo.clone(),
            node.issue.author_id,
            Labelable::Issue(node),
        ))
    }
}

async fn reload_labelable(api: &ApiContext, node: &Labelable) -> Result<Labelable> {
    Ok(match node {
        Labelable::Issue(issue) => Labelable::Issue(reload_issue(api, issue).await?),
        Labelable::PullRequest(pull) => Labelable::PullRequest(reload_pull(api, pull).await?),
    })
}

/// Change labels by adding or removing some (`add` true/false), with write access.
async fn change_labels(
    api: &ApiContext,
    input: LabelableInput,
    add: bool,
) -> Result<LabelablePayload> {
    let (issue_id, repo, _, node) = issue_or_pull_by_id(api, &input.labelable_id).await?;
    let user = require(api, &repo, Access::Write)?;
    let mut current: Vec<i64> = db::labels_by_issue(&api.state.db, repo.repo.id)
        .await
        .or_internal()?
        .remove(&issue_id)
        .unwrap_or_default()
        .iter()
        .map(|l| l.id)
        .collect();
    let changed: Vec<i64> = input
        .label_ids
        .iter()
        .filter_map(|id| parse_id(id, "LA").ok())
        .collect();
    if add {
        current.extend(
            changed
                .iter()
                .filter(|id| !current.contains(id))
                .collect::<Vec<_>>(),
        );
    } else {
        current.retain(|id| !changed.contains(id));
    }
    db::set_issue_labels(&api.state.db, repo.repo.id, issue_id, user.id, &current)
        .await
        .or_internal()?;
    notify(
        api,
        &repo,
        &node,
        if add { "labeled" } else { "unlabeled" },
        user,
    )
    .await;
    Ok(LabelablePayload {
        labelable: Some(reload_labelable(api, &node).await?),
        client_mutation_id: input.client_mutation_id,
    })
}

async fn change_assignees(
    api: &ApiContext,
    input: AssignableInput,
    add: bool,
) -> Result<AssignablePayload> {
    let (issue_id, repo, _, node) = issue_or_pull_by_id(api, &input.assignable_id).await?;
    let user = require(api, &repo, Access::Write)?;
    let mut current: Vec<i64> = db::assignees(&api.state.db, issue_id)
        .await
        .or_internal()?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let changed: Vec<i64> = input
        .assignee_ids
        .iter()
        .filter_map(|id| parse_id(id, "U").ok())
        .collect();
    if add {
        current.extend(
            changed
                .iter()
                .filter(|id| !current.contains(id))
                .collect::<Vec<_>>(),
        );
    } else {
        current.retain(|id| !changed.contains(id));
    }
    db::set_assignees(&api.state.db, issue_id, user.id, &current)
        .await
        .or_internal()?;
    notify(
        api,
        &repo,
        &node,
        if add { "assigned" } else { "unassigned" },
        user,
    )
    .await;
    Ok(AssignablePayload {
        assignable: Some(reload_labelable(api, &node).await?),
        client_mutation_id: input.client_mutation_id,
    })
}

/// Edit a title or description: the author or anyone with write access.
async fn edit_text(
    api: &ApiContext,
    id: &ID,
    title: Option<String>,
    body: Option<String>,
) -> Result<()> {
    let (issue_id, repo, author_id, node) = issue_or_pull_by_id(api, id).await?;
    let user = require(api, &repo, Access::Read)?;
    if repo.access < Access::Write && user.id != author_id {
        return Err(forbidden(&format!(
            "{} does not have the correct permissions to execute this action",
            user.username
        )));
    }
    if title.is_none() && body.is_none() {
        return Ok(());
    }
    let (old_title, old_body) = match &node {
        Labelable::Issue(i) => (i.issue.title.clone(), i.issue.body.clone()),
        Labelable::PullRequest(p) => (p.pull.title.clone(), p.pull.body.clone()),
    };
    let title = title.map(|t| t.trim().to_owned()).unwrap_or(old_title);
    if title.is_empty() {
        return Err(Error::new("The title can't be empty."));
    }
    db::update_issue_text(
        &api.state.db,
        issue_id,
        &title,
        body.as_deref().map_or(old_body.as_str(), str::trim),
    )
    .await
    .or_internal()?;
    notify(api, &repo, &node, "edited", user).await;
    Ok(())
}

/// Send the webhook event for an issue or pull request changing.
async fn notify(
    api: &ApiContext,
    repo: &RepositoryNode,
    node: &Labelable,
    action: &str,
    user: &db::User,
) {
    match node {
        Labelable::Issue(i) => {
            events::issue(&api.state, &repo.repo, i.issue.number, action, user).await
        }
        Labelable::PullRequest(p) => {
            events::pull_request(&api.state, &repo.repo, p.pull.number, action, user).await
        }
    }
}

#[Object]
impl Mutation {
    async fn add_pull_request_review(
        &self,
        ctx: &Context<'_>,
        input: AddPullRequestReviewInput,
    ) -> Result<AddPullRequestReviewPayload> {
        let api = api(ctx);
        let node = pull_by_id(api, &input.pull_request_id).await?;
        let user = require(api, &node.repo, Access::Read)?;
        let body = input.body.as_deref().unwrap_or_default().trim();
        let state = match input.event.unwrap_or(PullRequestReviewEvent::Comment) {
            PullRequestReviewEvent::Approve => "approved",
            PullRequestReviewEvent::RequestChanges => "changes_requested",
            PullRequestReviewEvent::Comment => "commented",
            PullRequestReviewEvent::Dismiss => {
                return Err(Error::new("Dismissing reviews isn't supported."))
            }
        };
        if user.id == node.pull.author_id && state != "commented" {
            return Err(Error::new("Can not approve your own pull request"));
        }
        if !node.pull.is_open() {
            return Err(Error::new("This pull request is closed."));
        }
        if state != "approved" && body.is_empty() {
            return Err(Error::new(
                "Review body is required for this kind of review.",
            ));
        }
        let tips = node.tips(&api.state).await?;
        db::add_review(
            &api.state.db,
            node.pull.issue_id,
            user.id,
            state,
            body,
            &tips.head_sha,
        )
        .await
        .or_internal()?;
        events::review(
            &api.state,
            &node.repo.repo,
            node.pull.number,
            state,
            body,
            user,
        )
        .await;
        let review = db::reviews(&api.state.db, node.pull.issue_id)
            .await
            .or_internal()?
            .pop();
        let collaborators = collaborator_names(api, &node.repo.repo).await?;
        Ok(AddPullRequestReviewPayload {
            pull_request_review: review.map(|review| Review {
                author: user_node(api, user),
                association: association(&node.repo.repo, &collaborators, &user.username),
                review,
            }),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn update_pull_request(
        &self,
        ctx: &Context<'_>,
        input: UpdatePullRequestInput,
    ) -> Result<PullRequestPayload> {
        let api = api(ctx);
        edit_text(api, &input.pull_request_id, input.title, input.body).await?;
        let node = pull_by_id(api, &input.pull_request_id).await?;
        if input
            .base_ref_name
            .as_ref()
            .is_some_and(|b| *b != node.pull.base_branch)
        {
            return Err(Error::new(
                "Changing a pull request's base branch isn't supported yet.",
            ));
        }
        if let Some(ids) = input.label_ids {
            change_labels(
                api,
                LabelableInput {
                    labelable_id: input.pull_request_id.clone(),
                    label_ids: Vec::new(),
                    client_mutation_id: None,
                },
                false,
            )
            .await?;
            change_labels(
                api,
                LabelableInput {
                    labelable_id: input.pull_request_id.clone(),
                    label_ids: ids,
                    client_mutation_id: None,
                },
                true,
            )
            .await?;
        }
        if let Some(ids) = input.assignee_ids {
            let user = require(api, &node.repo, Access::Write)?;
            let ids: Vec<i64> = ids.iter().filter_map(|id| parse_id(id, "U").ok()).collect();
            db::set_assignees(&api.state.db, node.pull.issue_id, user.id, &ids)
                .await
                .or_internal()?;
        }
        if let Some(state) = input.state {
            let open = state == PullRequestState::Open;
            set_pull_state(
                api,
                PullRequestIdInput {
                    pull_request_id: input.pull_request_id.clone(),
                    client_mutation_id: None,
                },
                open,
            )
            .await?;
        }
        Ok(PullRequestPayload {
            pull_request: Some(reload_pull(api, &node).await?),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn update_issue(
        &self,
        ctx: &Context<'_>,
        input: UpdateIssueInput,
    ) -> Result<IssuePayload> {
        let api = api(ctx);
        edit_text(api, &input.id, input.title, input.body).await?;
        let node = issue_by_id(api, &input.id).await?;
        if let Some(ids) = input.label_ids {
            let user = require(api, &node.repo, Access::Write)?;
            let ids: Vec<i64> = ids
                .iter()
                .filter_map(|id| parse_id(id, "LA").ok())
                .collect();
            db::set_issue_labels(
                &api.state.db,
                node.repo.repo.id,
                node.issue.issue_id,
                user.id,
                &ids,
            )
            .await
            .or_internal()?;
        }
        if let Some(ids) = input.assignee_ids {
            let user = require(api, &node.repo, Access::Write)?;
            let ids: Vec<i64> = ids.iter().filter_map(|id| parse_id(id, "U").ok()).collect();
            db::set_assignees(&api.state.db, node.issue.issue_id, user.id, &ids)
                .await
                .or_internal()?;
        }
        if let Some(state) = input.state {
            set_issue_state(api, &node, state == IssueState::Open).await?;
        }
        Ok(IssuePayload {
            issue: Some(reload_issue(api, &node).await?),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn add_labels_to_labelable(
        &self,
        ctx: &Context<'_>,
        input: AddLabelsToLabelableInput,
    ) -> Result<LabelablePayload> {
        change_labels(api(ctx), input.into(), true).await
    }

    async fn remove_labels_from_labelable(
        &self,
        ctx: &Context<'_>,
        input: RemoveLabelsFromLabelableInput,
    ) -> Result<LabelablePayload> {
        change_labels(api(ctx), input.into(), false).await
    }

    async fn add_assignees_to_assignable(
        &self,
        ctx: &Context<'_>,
        input: AddAssigneesToAssignableInput,
    ) -> Result<AssignablePayload> {
        change_assignees(api(ctx), input.into(), true).await
    }

    async fn remove_assignees_from_assignable(
        &self,
        ctx: &Context<'_>,
        input: RemoveAssigneesFromAssignableInput,
    ) -> Result<AssignablePayload> {
        change_assignees(api(ctx), input.into(), false).await
    }

    async fn create_pull_request(
        &self,
        ctx: &Context<'_>,
        input: CreatePullRequestInput,
    ) -> Result<CreatePullRequestPayload> {
        let api = api(ctx);
        let repo = repository_by_id(api, &input.repository_id).await?;
        let user = require(api, &repo, Access::Read)?;
        if input
            .head_repository_id
            .as_ref()
            .is_some_and(|id| *id != input.repository_id)
        {
            return Err(Error::new(
                "Pull requests between repositories aren't supported yet.",
            ));
        }
        let head = input
            .head_ref_name
            .rsplit(':')
            .next()
            .unwrap_or(&input.head_ref_name)
            .to_owned();
        let title = input.title.trim();
        if title.is_empty() {
            return Err(Error::new("A pull request needs a title."));
        }
        let git = repo.git(&api.state);
        let (base_sha, head_sha) = match actions::check_new_pull(
            &api.state,
            &repo.repo,
            &git,
            &input.base_ref_name,
            &head,
        )
        .await
        .or_internal()?
        {
            Ok(tips) => tips,
            Err(reason) => return Err(Error::new(reason)),
        };
        let number = db::create_pull(
            &api.state.db,
            &db::NewPull {
                repo_id: repo.repo.id,
                author_id: user.id,
                title,
                body: input.body.as_deref().unwrap_or_default().trim(),
                base_branch: &input.base_ref_name,
                head_branch: &head,
                base_sha: &base_sha,
                head_sha: &head_sha,
            },
        )
        .await
        .or_internal()?;
        events::pull_request(&api.state, &repo.repo, number, "opened", user).await;
        let pull = db::pull(&api.state.db, repo.repo.id, number)
            .await
            .or_internal()?;
        Ok(CreatePullRequestPayload {
            pull_request: pull.map(|pull| PullRequestNode { pull, repo }),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn merge_pull_request(
        &self,
        ctx: &Context<'_>,
        input: MergePullRequestInput,
    ) -> Result<MergePullRequestPayload> {
        let api = api(ctx);
        let node = pull_by_id(api, &input.pull_request_id).await?;
        let user = require(api, &node.repo, Access::Write)?.clone();
        let git = node.repo.git(&api.state);
        if let Some(expected) = &input.expected_head_oid {
            let tips = actions::pull_tips(&api.state, &git, &node.pull)
                .await
                .or_internal()?;
            if &tips.head_sha != expected {
                return Err(Error::new(
                    "Head branch was modified. Review and try the merge again.",
                ));
            }
        }
        let method = match input.merge_method.unwrap_or(PullRequestMergeMethod::Merge) {
            PullRequestMergeMethod::Merge => forge_git::MergeMethod::Merge,
            PullRequestMergeMethod::Squash => forge_git::MergeMethod::Squash,
            PullRequestMergeMethod::Rebase => forge_git::MergeMethod::Rebase,
        };
        if let Err(reason) =
            actions::merge_pull(&api.state, &node.repo.repo, &git, &node.pull, &user, method)
                .await
                .or_internal()?
        {
            return Err(Error::new(reason));
        }
        Ok(MergePullRequestPayload {
            pull_request: Some(reload_pull(api, &node).await?),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn close_pull_request(
        &self,
        ctx: &Context<'_>,
        input: ClosePullRequestInput,
    ) -> Result<PullRequestPayload> {
        set_pull_state(api(ctx), input.into(), false).await
    }

    async fn reopen_pull_request(
        &self,
        ctx: &Context<'_>,
        input: ReopenPullRequestInput,
    ) -> Result<PullRequestPayload> {
        set_pull_state(api(ctx), input.into(), true).await
    }

    async fn create_issue(
        &self,
        ctx: &Context<'_>,
        input: CreateIssueInput,
    ) -> Result<IssuePayload> {
        let api = api(ctx);
        let repo = repository_by_id(api, &input.repository_id).await?;
        let user = require(api, &repo, Access::Read)?.clone();
        let title = input.title.trim();
        if title.is_empty() {
            return Err(Error::new("An issue needs a title."));
        }
        let number = db::create_issue(
            &api.state.db,
            repo.repo.id,
            user.id,
            title,
            input.body.as_deref().unwrap_or_default().trim(),
        )
        .await
        .or_internal()?;
        let issue = db::issue(&api.state.db, repo.repo.id, number)
            .await
            .or_internal()?
            .ok_or_else(|| internal(anyhow::anyhow!("issue vanished")))?;
        // Labels and assignees need triage (write) access, as on the web.
        if repo.access >= Access::Write {
            if let Some(ids) = &input.label_ids {
                let ids: Vec<i64> = ids
                    .iter()
                    .filter_map(|id| parse_id(id, "LA").ok())
                    .collect();
                db::set_issue_labels(&api.state.db, repo.repo.id, issue.issue_id, user.id, &ids)
                    .await
                    .or_internal()?;
            }
            if let Some(ids) = &input.assignee_ids {
                let ids: Vec<i64> = ids.iter().filter_map(|id| parse_id(id, "U").ok()).collect();
                db::set_assignees(&api.state.db, issue.issue_id, user.id, &ids)
                    .await
                    .or_internal()?;
            }
        }
        events::issue(&api.state, &repo.repo, number, "opened", &user).await;
        Ok(IssuePayload {
            issue: Some(IssueNode { issue, repo }),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn close_issue(&self, ctx: &Context<'_>, input: CloseIssueInput) -> Result<IssuePayload> {
        let api = api(ctx);
        let node = issue_by_id(api, &input.issue_id).await?;
        set_issue_state(api, &node, false).await?;
        Ok(IssuePayload {
            issue: Some(reload_issue(api, &node).await?),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn reopen_issue(
        &self,
        ctx: &Context<'_>,
        input: ReopenIssueInput,
    ) -> Result<IssuePayload> {
        let api = api(ctx);
        let node = issue_by_id(api, &input.issue_id).await?;
        set_issue_state(api, &node, true).await?;
        Ok(IssuePayload {
            issue: Some(reload_issue(api, &node).await?),
            client_mutation_id: input.client_mutation_id,
        })
    }

    async fn add_comment(
        &self,
        ctx: &Context<'_>,
        input: AddCommentInput,
    ) -> Result<AddCommentPayload> {
        let api = api(ctx);
        let body = input.body.trim();
        if body.is_empty() {
            return Err(Error::new("A comment can't be empty."));
        }
        let (issue_id, repo, page_url, subject) = if input.subject_id.starts_with("PR_") {
            let node = pull_by_id(api, &input.subject_id).await?;
            let url = node.page_url(&api.state);
            (
                node.pull.issue_id,
                node.repo.clone(),
                url,
                CommentSubject::PullRequest(node),
            )
        } else {
            let node = issue_by_id(api, &input.subject_id).await?;
            let url = node.page_url(&api.state);
            (
                node.issue.issue_id,
                node.repo.clone(),
                url,
                CommentSubject::Issue(node),
            )
        };
        let user = require(api, &repo, Access::Read)?;
        db::add_comment(&api.state.db, issue_id, user.id, body, None)
            .await
            .or_internal()?;
        let number = match &subject {
            CommentSubject::Issue(i) => i.issue.number,
            CommentSubject::PullRequest(p) => p.pull.number,
        };
        events::comment(&api.state, &repo.repo, number, body, user).await;
        let comment = db::comments(&api.state.db, issue_id)
            .await
            .or_internal()?
            .into_iter()
            .rev()
            .find(|c| c.path.is_none())
            .ok_or_else(|| internal(anyhow::anyhow!("comment vanished")))?;
        let ids = user_ids(api).await?;
        let collaborators = collaborator_names(api, &repo.repo).await?;
        let cursor = comment.id.to_string();
        Ok(AddCommentPayload {
            comment_edge: Some(IssueCommentEdge {
                node: issue_comment(api, &ids, &collaborators, &repo.repo, &page_url, comment),
                cursor,
            }),
            subject: Some(subject),
            client_mutation_id: input.client_mutation_id,
        })
    }
}

/// Close or reopen: the author or anyone with write access, as on the web.
async fn set_pull_state(
    api: &ApiContext,
    input: PullRequestIdInput,
    open: bool,
) -> Result<PullRequestPayload> {
    let node = pull_by_id(api, &input.pull_request_id).await?;
    let user = require(api, &node.repo, Access::Read)?;
    if node.repo.access < Access::Write && user.id != node.pull.author_id {
        return Err(forbidden(&format!(
            "{} does not have the correct permissions to execute this action",
            user.username
        )));
    }
    if node.pull.is_merged() {
        return Err(Error::new(
            "A merged pull request can't be closed or reopened.",
        ));
    }
    if node.pull.is_open() != open {
        db::set_issue_state(&api.state.db, node.pull.issue_id, user.id, open)
            .await
            .or_internal()?;
        let action = if open { "reopened" } else { "closed" };
        events::pull_request(&api.state, &node.repo.repo, node.pull.number, action, user).await;
    }
    Ok(PullRequestPayload {
        pull_request: Some(reload_pull(api, &node).await?),
        client_mutation_id: input.client_mutation_id,
    })
}

async fn set_issue_state(api: &ApiContext, node: &IssueNode, open: bool) -> Result<()> {
    let user = require(api, &node.repo, Access::Read)?;
    if node.repo.access < Access::Write && user.id != node.issue.author_id {
        return Err(forbidden(&format!(
            "{} does not have the correct permissions to execute this action",
            user.username
        )));
    }
    if node.issue.is_open() != open {
        db::set_issue_state(&api.state.db, node.issue.issue_id, user.id, open)
            .await
            .or_internal()?;
        let action = if open { "reopened" } else { "closed" };
        events::issue(&api.state, &node.repo.repo, node.issue.number, action, user).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages() {
        let (items, page, total) = paginate((0..10).collect::<Vec<_>>(), Some(3), None, None);
        assert_eq!(
            (items, total, page.has_next_page, page.end_cursor.as_deref()),
            (vec![0, 1, 2], 10, true, Some("2"))
        );
        let (items, page, _) = paginate((0..10).collect::<Vec<_>>(), Some(3), None, Some("2"));
        assert_eq!((items, page.has_previous_page), (vec![3, 4, 5], true));
        let (items, page, _) = paginate((0..10).collect::<Vec<_>>(), None, Some(1), None);
        assert_eq!((items, page.end_cursor.as_deref()), (vec![9], Some("9")));
        let (items, page, _) = paginate(Vec::<i32>::new(), Some(5), None, None);
        assert!(items.is_empty() && !page.has_next_page && page.end_cursor.is_none());
    }

    #[test]
    fn ids() {
        assert_eq!(parse_id(&ID("PR_12".into()), "PR").unwrap(), 12);
        assert!(parse_id(&ID("I_12".into()), "PR").is_err());
        assert!(parse_id(&ID("PR_x".into()), "PR").is_err());
    }
}
