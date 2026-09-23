# lazyamp

Terminal UI for managing [Amp](https://ampcode.com/docs/cli) `--no-tui` runners. List, start, stop, restart, add/remove served directories, update the Amp CLI, and persist common start flags.

lazyamp does **not** reimplement Amp’s interactive agent TUI.

## Prerequisite

The Amp CLI must be on `PATH` (`amp --help` works). Install it from [ampcode.com/docs/cli](https://ampcode.com/docs/cli).

Override the binary with `AMP_BIN=/path/to/amp` if needed. lazyamp exits with a clear error if Amp is missing.

This repo was developed against the public Amp CLI contracts (Amp was not installed on the agent VM). Commands used:

| Action | Amp command |
| --- | --- |
| Start runner | `amp --no-tui` with optional `--runner-id`, `--mode`, `--log-level`, `--settings-file`, `--mcp-config`, `--visibility`, `--remote-control-terminal`, `--discover-dirs`, `--amp-env`, `--dir` |
| List runners | `amp runner list` (tries `--json` first, then text) |
| List / add / remove dirs | `amp runner dirs list\|add\|remove` (`--runner-id` when known) |
| Update CLI | `amp update` (Amp also accepts `amp up`) |

Related Amp settings (not edited by lazyamp): `amp.runner.autoUpdate.enabled`, `amp.updates.mode`.

## Install

Linux and macOS (`x86_64` / `aarch64`), from [GitHub Releases](https://github.com/miltonparedes/lazyamp/releases):

```bash
curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh
```

Installs to `~/.local/bin`, or `/usr/local/bin` if writable. Pin a version or prefix:

```bash
VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh -s -- --prefix /usr/local
```

The script verifies `sha256` from `checksums.txt` when that file is attached to the release.

From source:

```bash
cargo install --path .
```

Or download a `lazyamp-<version>-<target>.tar.gz` from [Releases](https://github.com/miltonparedes/lazyamp/releases) and put `lazyamp` on `PATH`.

## Usage

```bash
lazyamp
```

## Keybindings

| Key | Action |
| --- | --- |
| `hjkl` / arrows | Move; `h`/`l` also switch panes |
| `Tab` | Runners ↔ directories |
| `Enter` | Confirm overlay / show selected path |
| `q` / `Ctrl-c` | Quit |
| `?` | Help overlay |
| `Esc` | Close overlay |
| `s` | Start `amp --no-tui` (directory picker) |
| `x` | Stop selected runner |
| `r` | Restart selected runner |
| `g` | Refresh runner list |
| `a` | Add a served directory |
| `d` | Remove the selected directory |
| `f` | Common flags panel (saved to config) |
| `u` | Run `amp update` |
| `c` | Show config path |

Directory picker: type to filter, `Enter` to select, `→`/`l` to browse into a folder, `←`/`h` to go to the parent (when the filter is empty). Recent paths, cwd, and home are listed first.

## Config

Defaults and recent directories are stored at:

```
$XDG_CONFIG_HOME/lazyamp/config.toml
```

Usually `~/.config/lazyamp/config.toml`.

```toml
recent_dirs = ["/home/me/code"]

[defaults]
runner_id = "dev-box"
mode = "medium"
log_level = "info"
settings_file = "/path/to/settings.json"
mcp_config = "/path/to/mcp.json"
visibility = "workspace"
remote_control_terminal = false
discover_dirs = false
amp_env = false
```

Put `recent_dirs` before `[defaults]`. Values written after that table are treated as `defaults.recent_dirs` by TOML; lazyamp still reads them.

Runner logs and a spawn-PID registry live under `$XDG_STATE_HOME/lazyamp/` (usually `~/.local/state/lazyamp/`).

## PID detection

1. Parse a `pid` field from `amp runner list` (JSON keys `pid` / `processId`, or text like `pid 1234`).
2. If the list has no PIDs, scan this machine for `amp --no-tui` processes (Linux: `/proc/<pid>/cmdline` + `cwd`; other Unix: `ps`) and match on `--runner-id` or working directory.
3. Runners started from lazyamp are also recorded in `~/.local/state/lazyamp/spawned.json`.

Stop sends `SIGTERM`, then `SIGKILL` if the process is still alive. Started runners call `setsid()` so they keep running after you quit lazyamp.

## Releasing

`Cargo.toml` `version` must match the tag (for `0.1.0` use `v0.1.0`):

```bash
git tag v0.1.0
git push origin v0.1.0
```

That tag push builds Linux and macOS archives, publishes a GitHub Release, and uploads `checksums.txt`.

## Future

- Auto-updating lazyamp itself
- Orbs / Amp Net / thread browsing (out of scope)

## License

MIT
