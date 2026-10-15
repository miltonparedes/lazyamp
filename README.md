# lazyamp

Terminal UI for managing [Amp](https://ampcode.com/docs/cli) `--no-tui` runners. List, start, stop, restart, add/remove served directories, follow each runner's activity, update the Amp CLI and lazyamp itself, and persist common start flags. Keyboard first; the mouse works too.

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

Once installed, update in place with `lazyamp update` (or `U` inside the TUI). Releases up to v0.1.1 lack this command; re-run the install script once to get it.

From source (Rust 1.85+):

```bash
cargo install --path .
```

Or download a `lazyamp-<version>-<target>.tar.gz` from [Releases](https://github.com/miltonparedes/lazyamp/releases) and put `lazyamp` on `PATH`.

## Usage

```bash
lazyamp                          # open the TUI
lazyamp update                   # install the latest release if it is newer
lazyamp update --check           # show current and latest versions
lazyamp update --version v0.1.1  # install an exact release (rollback)
```

`lazyamp update` downloads that release's `install.sh` and runs it against the directory of the running binary, so the same sha256 verification and atomic rename apply. It needs `curl` and write access to that directory. **Running Amp runners are separate processes and keep running**; only lazyamp is replaced. Inside the TUI, a header badge shows when a newer release exists (checked at most once a day, off the UI thread; set `LAZYAMP_NO_UPDATE_CHECK=1` to disable). `U` installs it, then offers to quit so you can start the new version (lazyamp does not re-exec itself: that would orphan the threads that reap runners it started).

## Keybindings

The footer always shows the keys for the focused pane or dialog; `?` lists them all.

| Key | Action |
| --- | --- |
| `↑↓` / `jk` | Move |
| `Tab` / `←→` / `hl` | Switch between runners and directories |
| `s` | Start `amp --no-tui` (pick the process start cwd) |
| `x` | Stop the selected runner (confirms) |
| `r` | Restart the selected runner with the settings it was started with (confirms) |
| `a` | Add a served directory (`amp runner dirs add`) |
| `d` | Remove the selected served directory (confirms) |
| `Enter` | Show the selected directory's full path |
| `f` | Default flags for new runners (saved to config) |
| `u` | Run `amp update` |
| `U` | Update lazyamp itself |
| `g` | Refresh now |
| `L` | Message history |
| `c` | Show config path |
| `?` | Help |
| `q` / `Ctrl-c` | Quit |

Mouse: click a runner or directory to select it, click a footer hint or dialog button to press it, scroll to move, click outside a dialog to close it.

Directory picker: typing always searches (nested directories under the browse root, recent dirs, and `zoxide query -l` when `zoxide` is on `PATH`, zoxide first). `↑↓` move; `←→` browse up/into folders while the query is empty; `Tab` opens the highlighted folder; `Enter` picks it (or the typed path); `Esc` clears the query, then closes. Text fields have a real cursor (`←→`, `Home`/`End`, `Ctrl-a`/`Ctrl-e`, `Ctrl-u`). Heavy trees (`.git`, `node_modules`, `target`, …) are skipped and results are capped. The walk runs off the UI thread.

`s` chooses the runner's start cwd. `a` adds a served directory — a path the runner can see, which is not the same as the start cwd.

Removing the **primary directory** (start cwd) with `d` proposes the first remaining served directory that exists as the new primary. The confirmation names it and warns that this **restarts the runner and interrupts active threads**. No files are deleted. The restart keeps the runner ID, saved flags, and all remaining live served directories, including paths added after launch; relative settings/MCP file paths are resolved against the old cwd. The new launch spec is saved for later restarts.

This requires a saved launch spec, a known local PID, and another usable served directory. Removing the last directory or automatically changing cwd with `--discover-dirs` is refused before stopping anything. Other directory removals still use `amp runner dirs remove`; directories pinned by `--dir` need a manual launch change.

The right pane shows the selected runner's status, served directories (★ marks the start cwd), and **Activity**: the tail of the log file that process actually writes to (Linux resolves `/proc/<pid>/fd/1`; otherwise the spawn record). The thread count comes from Amp's latest `(N threads running)` line. A yellow dot and `↻` mean the runner still runs an amp binary that `amp update` has since replaced (Linux: `/proc/<pid>/exe` is ` (deleted)`); it picks up the new version when it restarts.

## Refresh

Refreshing is silent and never blocks actions. A background poller scans local `amp --no-tui` processes and their logs every 2 s (no subprocess) and runs `amp runner list` only every 15 s, when the set of local runner PIDs changes, after an action, or on `g`. Polling pauses while the terminal is unfocused (when the terminal reports focus). Only failures reach the status line, once until they change. Idle for 60 s, this spawns Amp 4 times instead of 19 and uses about a third of the CPU of v0.1.1.

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

Config and the spawn/launch registries are written atomically (temp file + rename). State files only gain optional fields, so state written by an older release keeps loading after an update, and a state file that cannot be parsed is never overwritten.

Runner logs, a spawn-PID registry (`spawned.json`), per-runner launch specs (`launches.json`), and the update-check cache (`update-check.json`) live under `$XDG_STATE_HOME/lazyamp/` (usually `~/.local/state/lazyamp/`). Restart replays how that exact process was started (the spawn record for its PID), then the saved launch spec by runner-id or cwd, never the current global defaults.

## PID detection

1. Parse a `pid` field from `amp runner list` (JSON keys `pid` / `processId`, or text like `pid 1234`). PIDs that are not a positive `i32` are ignored.
2. Scan this machine for `amp --no-tui` processes (Linux: `/proc/<pid>/cmdline` + `cwd` + parent PID; other Unix: `ps`). A binary named `amp` matches, and so does `node`/`bun`/`deno` running an Amp script. Processes are grouped by **runner-id and process tree** so wrapper/runtime children are one row, not three.
3. Amp list identity wins. Local helper PIDs attach to that row, but a process without `--runner-id` never attaches to a listed runner whose reported PID is a different process. The stop/restart PID is the listed PID when it is in the group, otherwise the tree root. Distinct runner-ids that share a working directory are not merged.
4. Runners started from lazyamp are also recorded in `~/.local/state/lazyamp/spawned.json`. Registry entries are kept only if the PID still looks like `amp --no-tui`.

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

That tag push builds musl Linux and macOS archives, publishes a GitHub Release, and uploads `checksums.txt`. `lazyamp update` resolves the latest release through the same `releases/latest` redirect as `install.sh`, then runs the `install.sh` from that tag, so a release needs no extra steps. Keep `scripts/install.sh` backward compatible: older lazyamp binaries pass `--version` and `--prefix` (or `--bin-dir` from v0.2.0).

## Future

- Orbs / Amp Net / thread browsing (out of scope)

## License

MIT
