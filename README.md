# lazyamp

A tiny terminal UI for babysitting your [Amp](https://ampcode.com/docs/cli) runners.

Start them, stop them, restart them, point them at folders, and watch what they're up to, without juggling `amp --no-tui` in five tmux panes.

## Install

You'll need the [Amp CLI](https://ampcode.com/docs/cli) on your `PATH` first. Then:

```bash
curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh
```

Works on Linux and macOS (x86_64 and arm64). Want a specific version? `... | VERSION=v0.2.0 sh`.

Prefer building it yourself? `cargo install --path .`

## Use

```bash
lazyamp                  # open it
lazyamp update           # grab the latest release
lazyamp update --check   # just look, don't touch
```

Once inside, the footer tells you what you can press. The greatest hits:

| Key | Does |
| --- | --- |
| `s` | start a runner |
| `x` | stop it |
| `r` | restart it |
| `a` / `d` | add / remove a directory |
| `f` | default flags for new runners |
| `u` | update Amp |
| `U` | update lazyamp |
| `?` | everything else |
| `q` | bye |

The mouse works too, if that's your thing.

Settings live in `~/.config/lazyamp/config.toml`.

## Orb development

Amp runs `.agents/setup` when preparing a project snapshot. It installs missing
Debian build prerequisites, stable Rust with rustfmt and Clippy (matching CI),
fetches `Cargo.lock` dependencies, and builds all targets. Warm setup reuses the
installed toolchain, crate cache, and Cargo build artifacts; it does not update
Rust automatically (`rustup update stable` does that when needed).

New login shells have Cargo on `PATH`. `.agents/resume` intentionally does nothing:
there are no services to restart or project credentials to restore. Orbs already
provide Amp; tests use fake Amp executables and need no login. Run the CI checks
with `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, and
`cargo test --locked`. The hooks must reach the project's default branch before
future orbs can use them.

## License

MIT
