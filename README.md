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
and `gh api`. omaforge has no projects, milestones, reactions or CI checks yet, so those
come back empty.

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
