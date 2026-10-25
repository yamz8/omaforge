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
- [ ] Branch protection basics (required review, no direct pushes)
- [x] Collaborators with read or write access (web settings and `admin add-collaborator`)
- [ ] Notifications, plus webhooks with GitHub-shaped payloads

## M2: Leave GitHub (or don't)
- [ ] `/api/v3` subset that `gh` needs for repo/issue/PR commands
- [ ] `omaforge import github`: git, issues, PRs, comments, labels, releases
- [ ] Sync modes: mirror-from, mirror-to, bidirectional (see architecture.md)

## M3: CI
- [ ] `omaforge-runner`: register, poll, run steps in containers, stream logs
- [ ] GitHub Actions YAML subset: `on: push/pull_request`, `jobs`, `runs-on`, `steps.run`, `env`, `needs`, matrix
- [ ] `uses:` support for common actions (checkout, cache, setup-*)
- [ ] Commit statuses / checks shown on PRs

## M4: Agents
- [ ] Bot users + scoped tokens
- [ ] PR review agent (posts a normal review)
- [ ] Issue triage (labels, duplicates)
- [ ] "Assign to agent": the agent opens a PR for an issue

## Later
- Orgs/teams, releases + artifacts, packages, wiki, Postgres, an `oma` CLI and desktop client, and federation (ForgeFed).
