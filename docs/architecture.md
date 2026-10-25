# omaforge architecture

omaforge is a self-hosted, single-tenant git forge: one install per person or team,
one binary, one data directory. It aims to replace GitHub (and Gitea/Forgejo) for
teams that want something simpler, faster and better looking — while staying
compatible enough with GitHub that existing tools, workflows and repos keep working,
and optionally staying in sync with GitHub.

## Principles

- **One binary, one directory.** `omaforge serve` runs web, git (HTTP + SSH),
  background jobs and the API. All state lives under `$OMAFORGE_DATA`
  (`repos/`, `omaforge.db`, `attachments/`, `ci/`).
- **Git is the source of truth for code.** Bare repos on disk; we never mirror git
  objects into the database.
- **Server-rendered UI.** HTML templates + htmx, small amounts of JS. Fast by default.
- **GitHub-shaped where it matters.** REST under `/api/v3` and GraphQL at
  `/api/graphql` follow GitHub's shapes for the endpoints we implement, so `gh`
  (via `GH_HOST`), editors and scripts work against omaforge.
- **Agents are users.** AI agents are bot accounts with scoped tokens. They comment,
  review and push through the same paths a person would, with nothing hidden.

## Components

| Component | Responsibility | Key crates |
|---|---|---|
| `omaforge` (bin) | CLI entry: `serve`, `init`, `admin`, `import`, `sync` | clap, tokio |
| web | Routes, auth/sessions, HTML views; Markdown (GFM, sanitized), syntax highlighting, theme | axum, askama, comrak + ammonia, syntect (+ two-face), tower-http |
| `forge-git` | Read repos (trees, blobs, commits, diffs, blame), refs, merges | the `git` CLI, behind a small API so gitoxide can replace hot paths later |
| git transport | Smart HTTP (`git-upload-pack`/`git-receive-pack --stateless-rpc`); SSH server that dispatches to the same | axum, russh |
| db | Users, orgs, repos metadata, issues, PRs, reviews, CI runs, sync mappings | sqlx + SQLite (Postgres later) |
| jobs | Durable queue in the DB: webhooks, sync, CI dispatch, agent tasks, indexing | tokio tasks over a `jobs` table |
| `forge-ci` | Workflow files (a GitHub Actions subset that grows over time), `${{ }}` expressions, GitHub's filter globs, and the runner protocol; shared by the server and the runner | yaml-rust2 |
| CI (in `omaforge`) | Starts runs for push and pull request events, queues jobs (`needs`, matrix, fail-fast), hands them to runners, stores step logs, reports checks | |
| `omaforge-runner` (bin) | Claims jobs over HTTP with a runner token, runs each step on the host or in a Docker container, streams logs back; checks out code itself with a per-job token | reqwest, the `docker` CLI |
| api | GitHub-compatible REST (`/api/v3`) and GraphQL (`/api/graphql`) subset for `gh` and scripts; shares merge/permission logic with the web through `actions` | async-graphql |
| GitHub bridge | One-time import and continuous two-way sync | octocrab |
| agents | Bot users driven by the Claude API: PR review, issue triage, "fix this" tasks | reqwest |

### Request paths

```
browser ──HTTP──▶ axum ─┬─ HTML views ──▶ forge-git / db
                        ├─ /api/v3, /api/graphql (GitHub-compatible)
                        └─ /{owner}/{repo}.git/* ──▶ git upload-pack / receive-pack
ssh client ──SSH──▶ russh ──auth (keys in db)──▶ git upload-pack / receive-pack
receive-pack ──pre-receive hook──▶ branch protection (refuse with a reason)
             ──post-receive hook──▶ events ─┬─▶ deliveries queue → webhook worker
                                            └─▶ CI: workflow runs → queued jobs
omaforge-runner ──long poll /api/runner/v1/claim──▶ a job + a job token
                ──job token──▶ clone, step updates, logs, finish → next jobs, checks
```

### CI

A push or pull request event reads the workflow files at that commit, matches each one's
`on:` against the event (path filters compare against the push's `before`, a new branch's
merge base with the default branch, or a pull request's merge base), and records a run
with one job per matrix combination. Jobs that need nothing are queued; the rest wait,
and when their needs finish their `if:` (default `success()`) queues or skips them.

Runners authenticate with a runner token (`omr_…`, stored hashed) and long-poll for a
queued job whose `runs-on` labels they all have. Each claimed job gets its own token
(`omj_…`), valid only while it runs, which lets the runner report on that job and clone
that one repository; nothing else. The runner evaluates steps itself (expressions, env,
`if:`), so the server only needs the workflow, the event payload and the results of
needed jobs. Logs are stored per step under `<data>/ci/logs/`. A runner that stops
heartbeating has its job failed by a reaper.

A runner label says where its jobs run: `NAME=host` runs steps directly on the runner's
machine, `NAME=docker://IMAGE` in a container (one per job, with the workspace mounted;
a workflow's `container:` picks the image but can't add Docker options). Checkout always
happens on the runner's side, so images don't need git.

```text
```

## GitHub migration and sync

**Import** (`omaforge import github <owner/repo|org>`): clones the git data, then
copies issues, PRs (as PRs with their head refs kept), comments, reviews, labels,
milestones and releases through the GitHub API. Authors are matched to local users
by email or GitHub login, or become placeholder "ghost" users that can be claimed
later.

**Sync** (per repo, opt-in), with three modes:

1. `mirror-from-github`: GitHub stays canonical, and omaforge pulls git and
   issues/PRs on webhook or poll.
2. `mirror-to-github`: omaforge is canonical, and it pushes refs and mirrors
   issues/PRs out.
3. `bidirectional`: both sides take writes.
   - Git: refs are fast-forward synced. If the two sides diverge, sync for that ref
     stops and a conflict banner appears on the repo. Nothing is force-pushed
     automatically.
   - Issues, PRs and comments: a `sync_links` table maps local IDs to remote node
     IDs. Each change is stamped with its origin so it isn't echoed back. When both
     sides edit the same field, the last writer wins, and the losing version is kept
     in history.

## Omarchy integration

- The UI's colors are CSS variables generated from an Omarchy `colors.toml`. By default
  omaforge follows the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`)
  and re-reads it on every request, so `omarchy theme set` shows up on reload. Without
  Omarchy it uses built-in Tokyo Night (dark) and Catppuccin Latte (light).
  `--theme builtin|omarchy|<path>` overrides this.
- An `oma` CLI handles clone, PRs, issues and CI logs. It may wrap `gh` (pointed at
  omaforge) at first.
- A native Qt desktop client can come later, built the Omarchy-app way.

## Data model (initial)

`users`, `ssh_keys`, `tokens`, `orgs`, `org_members`, `repos`, `collaborators`,
`issues` (shared by issues and PRs, numbered per repo like GitHub), `pull_requests`,
`comments`, `reviews`, `review_comments`, `labels`, `issue_labels`, `webhooks`,
`deliveries`, `branch_rules`, `runners`, `workflow_runs`, `ci_jobs`, `ci_steps`,
`commit_statuses`, `sync_configs`, `sync_links`.
