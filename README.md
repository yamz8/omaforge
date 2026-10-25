# omaforge

A self-hosted git forge, written in Rust. It is simple, fast and Omarchy-themed, and compatible with GitHub: you can import from it, sync with it, and point `gh` at it.

- [Architecture](docs/architecture.md)
- [Roadmap](docs/roadmap.md)

## Development

```sh
cargo run -- admin create-user alice --admin
cargo run -- admin create-repo alice/hello
cargo run -- admin create-token alice laptop   # prints omf_… token
cargo run -- admin add-ssh-key alice laptop --key-file ~/.ssh/id_ed25519.pub
cargo run -- serve                             # web + HTTP git :3000, SSH :2222

git push http://alice:<token>@127.0.0.1:3000/alice/hello.git main
git push ssh://git@127.0.0.1:2222/alice/hello.git main
```

Import repositories you already have on disk (private by default; re-run with
`--update` to bring in new commits, which only ever fast-forwards):

```sh
cargo run -- import local ~/code/* --owner alice --dry-run
cargo run -- import local ~/code/* --owner alice
```

### Using the `gh` CLI

omaforge speaks enough of GitHub's API (REST at `/api/v3`, GraphQL at `/api/graphql`,
GitHub Enterprise's paths) for `gh` to work. Behind HTTPS on its own hostname:

```sh
gh auth login --hostname forge.example.com --with-token < token.txt   # an omf_ access token
cd my-clone && gh pr create            # or: GH_HOST=forge.example.com gh pr list -R you/repo
```

Tested: `gh repo view/clone`, `gh issue create/list/view/comment/edit/close/reopen`,
`gh pr create/list/view/diff/checks/status/edit/review/comment/merge/close/reopen`,
`gh run list/view/rerun/cancel` (with `--log` and `--log-failed`), and `gh api`.
omaforge has no projects, milestones or reactions yet, so those come back empty.

### CI

Put GitHub Actions-style workflows in `.omaforge/workflows/` or `.github/workflows/`
(see the [roadmap](docs/roadmap.md) for what's supported). Jobs run on
`omaforge-runner`, on any machine that can reach the server:

```sh
cargo run -- admin create-runner build-box        # prints an omr_… runner token
export OMAFORGE_RUNNER_TOKEN=omr_…                # kept out of the process list
cargo run -p omaforge-runner -- --server http://127.0.0.1:3000 \
    --label ubuntu-latest=docker://ubuntu:24.04 \
    --label self-hosted=host
```

Each `--label` says where jobs with that label run: in a Docker container from the
image, or (`=host`) directly on the runner's machine as the runner's user. Host jobs
aren't isolated from the runner, so they could read its token and pick up other
repositories' jobs: only give a runner host labels if you trust everyone who can push
workflows. The Docker executor needs the runner's user to be able to use Docker
(`sudo usermod -aG docker $USER`). Site admins can also add runners and see which are
online under Settings → CI runners.

Results show up on the Actions tab, as checks on pull requests and commits, and through
`gh`. Branch protection can require checks to pass before merging, and other CI systems
can report results with the commit statuses API (`POST /api/v3/repos/{owner}/{repo}/statuses/{sha}`).

Run the end-to-end tests (real server, real git over HTTP and SSH) with:

```sh
cargo build && tests/e2e.sh
```

Open http://127.0.0.1:3000 to browse, and sign in to see private repos, create
repositories and manage SSH keys and access tokens under Settings. Accounts are
created with `admin create-user`. The UI follows your active Omarchy theme
(`--theme builtin|omarchy|<colors.toml>` to choose), and `--public-url` sets the
URL used in clone instructions.

Data lives in `./data` (override with `--data` or `OMAFORGE_DATA`).

Requires a Rust toolchain (`rustup`) and `git`.
