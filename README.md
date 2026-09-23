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

Linux and macOS (`x86_64` / `aarch64`), from [GitHub Releases](https://github.com/miltonparedes/lazyamp/releases).

Linux archives are **musl static** binaries (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) so they run on older glibc distros (Ubuntu 22.04, Debian 12, RHEL 9, …). `install.sh` always prefers those musl artifacts.

```bash
curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh
```

Installs to `/usr/local/bin` if writable, otherwise `~/.local/bin`. Pin a version or prefix. **`VERSION` must be set on `sh`**, not on `curl`:

```bash
curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | VERSION=v0.1.1 sh
curl -fsSL https://raw.githubusercontent.com/miltonparedes/lazyamp/main/scripts/install.sh | sh -s -- --prefix /usr/local
```

The script also reads `VERSION` from the environment when you run `install.sh` directly (`VERSION=v0.1.1 ./scripts/install.sh`). It requires `checksums.txt` and verifies sha256 (set `LAZYAMP_INSECURE_SKIP_VERIFY=1` only as an escape hatch). The binary is staged then renamed to avoid `Text file busy`.

From source (Rust 1.85+):

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
| `x` | Stop selected runner (confirms) |
| `r` | Restart selected runner (confirms; uses that runner's launch spec) |
| `g` | Refresh runner list |
| `a` | Add a served directory |
| `d` | Remove the selected directory (confirms) |
| `f` | Common flags panel (saved to config) |
| `u` | Run `amp update` |
| `c` | Show config path |

Directory picker: `/` or any non-`hjkl` key starts filter mode (typed text is shown; `hjkl` then insert as letters). Arrows always move. `Enter` selects the highlighted match. `Esc` clears the filter first, then closes. Recent paths, cwd, and home are listed first.

The right pane shows the last lines of the selected runner's log (under `$XDG_STATE_HOME/lazyamp/logs/`). Amp CLI calls run off the UI thread; the status line shows `working…` while they run.

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

A **missing** config file uses defaults and may be created on a successful quit. An **invalid** `config.toml` is reported as an error and is never overwritten with defaults.

Config and the spawn/launch registries are written atomically (temp file + rename).

Runner logs, a spawn-PID registry (`spawned.json`), and per-runner launch specs (`launches.json`) live under `$XDG_STATE_HOME/lazyamp/` (usually `~/.local/state/lazyamp/`). Restart replays the saved launch spec (cwd, runner-id, flags), not the current global defaults.

## PID detection

1. Parse a `pid` field from `amp runner list` (JSON keys `pid` / `processId`, or text like `pid 1234`). PIDs that are not a positive `i32` are ignored.
2. If the list has no PIDs, scan this machine for `amp --no-tui` processes (Linux: `/proc/<pid>/cmdline` + `cwd`; other Unix: `ps`) and match on **runner-id or the exact PID**. Distinct runners that share a working directory are not merged.
3. Runners started from lazyamp are also recorded in `~/.local/state/lazyamp/spawned.json`. Registry entries are kept only if the PID still looks like `amp --no-tui`.

Stop signals **only the exact process PID** — never `kill(-pid)` / process-group broadcast. Before `SIGTERM`/`SIGKILL`, lazyamp verifies identity:

- Linux: `/proc/<pid>/exe` and cmdline must look like `amp --no-tui` (and `--runner-id` must match when both sides have one).
- macOS and other Unix: only the `ps` command line is available. That can be rewritten or truncated, so identity is weaker than on Linux.

Stop succeeds only after the process is gone (or `ESRCH`). `EPERM` or a still-living process is reported as a failure. `pid <= 1` is refused.

Start does not report “healthy” until the process is still alive and either appears in `amp runner list` or is clearly waiting for Amp login. Early exits and login prompts point at the runner log file.

## Releasing

`Cargo.toml` `version` must match the tag (for `0.1.1` use `v0.1.1`):

```bash
git tag v0.1.1
git push origin v0.1.1
```

That tag push builds musl Linux and macOS archives, publishes a GitHub Release, and uploads `checksums.txt`.

## Future

- Auto-updating lazyamp itself
- Orbs / Amp Net / thread browsing (out of scope)

## License

MIT
