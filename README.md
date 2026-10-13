# openclaw-rs

A single-binary Rust rewrite of OpenClaw: a personal AI companion that runs
on a Raspberry Pi or any Linux machine. It talks to models on OpenRouter or a
ChatGPT plan, keeps sessions and memories in SQLite, uses tools (shell,
files, web search, browser, MCP, skills, image generation), and chats
through the terminal, a Web UI, QQ, Feishu, Zulip and email. Everything is
built in; there are no plugins.

Full documentation: [docs/guide.md](docs/guide.md).

## Install

On any x86_64 or aarch64 Linux (Raspberry Pi with a 64-bit OS included),
from GitHub Releases:

```sh
curl -fsSL https://raw.githubusercontent.com/chappie220/openclaw/main/scripts/install.sh | sh
```

The script picks the binary for this machine, checks its checksum, installs
it and runs `openclaw-rs init` for first-time setup. Later updates:
`openclaw-rs update`.

## Build

```sh
cargo build --release
```

A static binary for a Raspberry Pi on Alpine (musl), cross-compiled from any
Linux host:

```sh
rustup target add aarch64-unknown-linux-musl
pip install ziglang && cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

The binary is `target/<target>/release/openclaw-rs`. On the Pi itself:
`apk add cargo build-base && cargo build --release`.
