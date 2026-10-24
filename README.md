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
cargo run -- serve                             # HTTP :3000, SSH :2222

git push http://alice:<token>@127.0.0.1:3000/alice/hello.git main
git push ssh://git@127.0.0.1:2222/alice/hello.git main
```

Run the end-to-end tests (real server, real git over HTTP and SSH) with:

```sh
cargo build && tests/e2e.sh
```

Data lives in `./data` (override with `--data` or `OMAFORGE_DATA`).

Requires a Rust toolchain (`rustup`) and `git`.
