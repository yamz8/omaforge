# omaforge roadmap

Each milestone should be something you can use day to day before the next one starts.

## M0: Host your code
- [x] `omaforge init` / `serve`, SQLite migrations
- [x] Users (CLI), password + personal access token auth
- [x] SSH keys (CLI and web settings)
- [x] Web sign-in: sessions, CSRF-protected forms, settings for SSH keys, tokens and password
- [x] Create repositories from the web
- [x] Create repos (bare, on disk), with public/private visibility (CLI)
- [x] Clone/push over smart HTTP (protocol v0 and v2)
- [x] Clone/push over SSH (built-in server, protocol v0 and v2)
- [x] Code browser: tree, file view with syntax highlighting, README rendering, commits, a single commit diff, branches/tags
- [x] Omarchy-themed UI (follows the active Omarchy theme live)
- [x] Code browser for private repos (owner and admins)
- [ ] Blame, compare views, syntax-highlighted diffs
- [x] `omaforge import local <dirs…>`: bulk-import existing local repositories (`--update` to re-sync)
- [x] Repository settings on the web (description, visibility, collaborators, rename, delete)

## M1: Collaborate
- [x] Issues: markdown, labels, assignees, comments, references (`#12`, `@name`, `fixes #12` closes on merge)
- [x] Pull requests: compare, create, diff view, inline review comments, approve/request changes
- [x] Merge: merge commit, squash, rebase (in the bare repo, compare-and-swap on the branch); delete merged branch
- [x] Branch protection: require pull requests, required approvals (from writers), required checks, block force pushes and deletion; enforced by a pre-receive hook
- [x] Collaborators with read or write access (web settings and `admin add-collaborator`)
- [x] Webhooks with GitHub-shaped, signed payloads (push, pull_request, pull_request_review, issues, issue_comment, create, delete, workflow_run, status), retries and redelivery
- [ ] Notifications

## M2: Leave GitHub (or don't)
- [x] `/api/v3` + `/api/graphql` subset that `gh` needs for repo/issue/PR commands (tested with real `gh`)
- [ ] `omaforge import github`: git, issues, PRs, comments, labels, releases
- [ ] Sync modes: mirror-from, mirror-to, bidirectional (see architecture.md)

## M3: CI
- [x] `omaforge-runner`: registered by token, long-polls for jobs, runs steps on the host or in Docker containers, streams logs (live in the web UI), heartbeats; jobs of runners that vanish are failed
- [x] GitHub Actions YAML subset: `on: push/pull_request` (branch, tag and path filters, `types`), `jobs`, `runs-on` labels, `steps` (`run`, `shell`, `working-directory`, `if`, `continue-on-error`, `timeout-minutes`), `env`, `needs` with outputs, matrix (`include`/`exclude`, fail-fast), `${{ }}` expressions, `GITHUB_OUTPUT`/`ENV`/`PATH`, `::add-mask::`
- [x] Workflow files in `.omaforge/workflows/` or `.github/workflows/`; invalid files show up as failed runs with the reason
- [x] Actions pages: runs, jobs, step logs; cancel and re-run; runner admin page
- [x] Checks on pull requests and commits; commit statuses API for other CI systems; `gh run list/view/rerun/cancel`, `gh run view --log(-failed)`, `gh pr checks`
- [ ] `uses:` for actions beyond the built-in `actions/checkout` (cache, setup-*, JavaScript and Docker actions)
- [ ] Secrets and variables, `workflow_dispatch` and `schedule`, artifacts, reusable workflows, services

## M4: Agents
- [ ] Bot users + scoped tokens
- [ ] PR review agent (posts a normal review)
- [ ] Issue triage (labels, duplicates)
- [ ] "Assign to agent": the agent opens a PR for an issue

## Later
- Orgs/teams, releases + artifacts, packages, wiki, Postgres, an `oma` CLI and desktop client, and federation (ForgeFed).
