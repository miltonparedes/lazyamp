# lazyamp design review — 2026-09-23

Reviewer: Opus 5.5 (cloud agent). Scope: TUI design, interaction model, tool architecture, product fit.
Not a security audit, but a few process-safety findings are P0 because they can kill unrelated processes.

Reviewed `main` at `7fb12b0` (MVP #1 + release CI #2), v0.1.0 as published on GitHub Releases.

## How this was reviewed

- Read all of `src/` (`amp.rs`, `config.rs`, `ui.rs`, `main.rs`), `scripts/install.sh`, both workflows, README.
- `cargo test --locked` (31 pass) and a release build on rustc 1.98.
- Drove the TUI in tmux (110×32 and 80×24) against:
  - a scriptable fake `amp` (JSON list, text list, crash-on-start, slow list), and
  - the **real Amp CLI** `0.0.1790142911-g7618fb`, installed from ampcode.com, signed out.
- Real-CLI facts that change the picture (from `amp runner --help`, `amp runner list --json`, and the
  [Runners docs](https://ampcode.com/docs/cli/runners)):
  - `amp runner list` exists, is **scoped to this machine** ("List the running runners on this machine"),
    and supports `--json` (`{"runners": []}` when empty; text: `No running amp --no-tui runner with a control socket on this machine.`).
  - `amp runner dirs remove` only removes "a directory added with `amp runner dirs add`".
  - `--runner-id` on `dirs` commands is "required when several are running".
  - One runner can serve many directories (`--dir`, `--discover-dirs[=path]`, `--discover-depth`,
    `--discover-exclude`, `--no-serve-cwd`); added dirs are remembered **per start directory**.
  - Runners self-update and restart themselves, keeping ID, dirs, and flags.
  - Signed out, `amp --no-tui` prints `Would you like to log in to Amp? [(y)es, (n)o]:` and waits forever,
    even with stdin = `/dev/null`.
- Not verified: the JSON shape of a *live* runner in `amp runner list --json` (needs a signed-in machine).
  Capturing that fixture is the first v0.2 task (see P1-7).

---

## P0 — must fix before promoting the curl install widely

### P0-1. `x` / `r` can kill every process the user owns

`amp::send_signal` sends the signal to the PID **and** to `-pid` (the process group):

```rust
libc::kill(pid as i32, sig);
libc::kill(-(pid as i32), sig);
```

With `pid == 1`, the second call is `kill(-1, SIGTERM)`, which is a broadcast to every process the user can
signal. After 2.5 s `stop_pid` follows up with `kill(-1, SIGKILL)`.

**Reproduced:** a fake `amp runner list --json` returned `{"id":"mac-mini","pid":1}`. I ran lazyamp as a
throwaway user that owned one unrelated `sleep 9999` and pressed `j` `x`. The `sleep` was killed. The log
line read `stopped mac-mini (pid 1)`, reported as a success. On a desktop this takes down the shell, editor,
and SSH sessions.

Real `amp runner list` is local-only, so a PID from another host is unlikely. The same trust problem still
exists in two places:

- `load_spawn_registry` treats any `spawned.json` PID that passes `kill(pid, 0)` as a live runner. After a
  reboot or PID reuse, an unrelated process shows up as a runner, and `x` signals it and its process group.
- Any PID parsed from list output (`first_pid`, `extract_pid`) is trusted as-is.

**Fix (small, local to `amp.rs`):**

1. Add `fn verify_local_amp(pid) -> Option<LocalProcess>`, which re-reads `/proc/<pid>/cmdline` on Linux or
   `ps -p <pid> -o command=` elsewhere, and requires `parse_amp_cmdline` to match. Call it in
   `load_spawn_registry`, and immediately before every signal in `stop_pid`.
2. Never signal `pid <= 1`. Only signal the group when `getpgid(pid) == pid` **and** the process was spawned
   by lazyamp (registry hit), since that's the only case where `setsid()` guarantees the group is ours.
3. Store the process start time in `SpawnRecord` (`/proc/<pid>/stat` field 22, or `ps -o lstart=`) and treat
   a mismatch as "not ours".

### P0-2. A config typo silently deletes the user's config

`ui::run` calls `Config::load().unwrap_or_default()`, and `app_loop` always calls `config.save_to(...)` on
quit. When `config.toml` fails to parse, lazyamp runs on defaults without telling anyone, then overwrites
the file on exit.

**Reproduced:** a hand-written config with one unclosed quote (plus comments, `recent_dirs`, and
`mode = "high"`) was replaced on quit by:

```toml
recent_dirs = []

[defaults]
remote_control_terminal = false
discover_dirs = false
amp_env = false
```

**Fix:** on a parse error, log it prominently (red status and log line naming the file and TOML error) and
set `App.config_writable = false` so nothing is written back. Better still, refuse to start and print the
error, like the missing-Amp path does. Separately (P1-10), only write when something changed.

### P0-3. Restart doesn't restart *that* runner

`restart_selected` stops the PID, then starts with `self.start_options()`, which are the **current global
defaults**:

- If `defaults.runner_id` is set, it **replaces** the runner's own ID. The fallback to
  `runner.runner_id_flag()` only applies when the defaults have no ID.
- `mode`, `log_level`, `visibility` and the other flags come from the defaults, not from the running process.
- `extra_dirs` is always empty, so `--dir` / `--discover-dirs=...` from the original launch are dropped.
- The working directory is `runner.cwd.or(dirs.first())`. When the cwd is unknown (macOS `ps` path, or list
  JSON without cwd), it restarts in whichever served dir is listed first. Amp remembers `dirs add` dirs **per
  start directory**, so this also silently drops the runner's added dirs.

**Reproduced:** started a runner as `--runner-id alpha --mode high`, changed the defaults to `beta` / `low`,
and pressed `r`. The log read `restarted dev-box in /tmp`, but Amp was invoked with
`--no-tui --runner-id beta --mode low`. Three different names for one process.

That last point is a separate bug: `find_match` falls back to cwd matching, so the `alpha` process was merged
into the `dev-box` row because both served `/tmp`.

**Fix:** restart must replay the process's **actual argv and cwd**. Read them from `/proc/<pid>/cmdline` and
`/proc/<pid>/cwd` (macOS: `ps -o command=` plus `lsof -a -d cwd -p <pid> -Fn`), or store the full argv in
`SpawnRecord` at start time. Defaults only apply to *new* starts. This matches Amp's own self-update restart,
which "keeps the runner ID, the served directories, and the other command-line flags". Also drop
cwd-only matching in `find_match` when both sides have a runner ID and the IDs differ.

### P0-4. "started" is logged for runners that never come up

`AmpClient::start_runner` returns as soon as `spawn()` succeeds, and the reaper thread discards the exit
status. The UI then logs `started <id> in <dir> (pid N)` in green.

**Reproduced against the real Amp CLI (signed out):** `s` `Enter` logged `started lz-review in /tmp/rt (pid 6775)`.
The runner row showed `lz-review  pid 6775  1 dir  [local]`, which looks healthy. Meanwhile
`~/.local/state/lazyamp/logs/lz-review.log` contained:

```
No API key found. Starting login flow...
Would you like to log in to Amp? [(y)es, (n)o]:
```

The process sits at that prompt forever. With the fake crash-on-start Amp, the row just vanishes on the
next refresh, with no error anywhere.

This is the first thing a new user hits after `curl | sh`, so it's P0.

**Fix:**

- Give every row a health state (see P1-4 for the UI):
  - `connected`: present in `amp runner list --json`, which means the control socket is up.
  - `starting…`: a local process exists but isn't listed yet, less than 10 s old.
  - `not connected`: process exists, not listed, older than 10 s.
  - `exited (code)`: pass the exit status from the reaper thread back over a channel.
- On `not connected` or `exited`, show the last 5 lines of the runner log inline and in the log pane.
  Special-case `No API key found` → "Amp is signed out. Run `amp login` in a terminal, then press `r`."
- Kill our own spawned process if it's still at the login prompt after the timeout, since nothing can ever
  answer it.

### P0-5. The Linux release binary doesn't run on common LTS distros

The release builds `x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu` on `ubuntu-latest` (24.04).
The v0.1.0 artifact requires `GLIBC_2.39`:

```
$ objdump -T lazyamp | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1
GLIBC_2.39
```

That fails with `GLIBC_2.39 not found` on Ubuntu 22.04 (2.35), Debian 12 (2.36), RHEL/Alma/Rocky 9 (2.34),
and Amazon Linux 2023 (2.34). That's a large share of the "old box in the garage" machines runners are for.

**Fix:** ship `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. The dependencies are pure Rust
plus `libc`, so a static build is straightforward (`cross`, or `musl-tools` + `aarch64-linux-musl` gcc). Then
make `install.sh` map `Linux` to `unknown-linux-musl`. This is a release-workflow change, so I left it for a
follow-up PR rather than making it here.

### P0-6. The documented version pin doesn't pin *(fixed in this PR)*

README and the `install.sh` header say:

```bash
VERSION=v0.1.0 curl -fsSL .../install.sh | sh
```

That assignment only applies to `curl`, so `sh` never sees `VERSION` and installs latest. Verified:
`VERSION=v9.9.9 true | sh -c 'echo ${VERSION:-unset}'` prints `unset`. The correct form is
`curl -fsSL .../install.sh | VERSION=v0.1.0 sh`. This PR fixes the README and script header. The release
body template (`Pin a version with VERSION=vX`) is ambiguous but not wrong, so `release.yml` is untouched.

---

## P1 — should fix soon

### P1-1. Every Amp call blocks the UI thread

`App::refresh`, `start_in`, `stop_selected` (up to 2.5 s of `sleep` in `stop_pid`), `add_dir`,
`remove_selected_dir`, and `run_update` all run synchronously inside `handle_key` or the 3 s refresh tick.
Measured with the real CLI: `amp version` takes 382 ms and `amp runner list --json` 375 ms. So:

- **Startup:** a blank alternate screen for about 0.75 s. `App::new` calls `version()` and `refresh()`
  before the first `draw`.
- **Steady state:** input freezes for about 0.4 s every 3 s. With a 2 s list, `?` took about 2.5 s to render.
- **`u`:** `amp update` runs to completion with a frozen UI. The `pending_update` trick paints the status
  line once, but there's no spinner and no cancel.

**Fix:** one worker thread with `mpsc::Sender<Cmd>` / `Receiver<Msg>`
(`Cmd::{Refresh, Start, Stop, AddDir, RemoveDir, Update}` → `Msg::{Runners(..), Done(..), Failed(..)}`).
Draw immediately with a `Loading runners…` placeholder. Keep a `busy: Option<(&'static str, Instant)>`
spinner in the status bar. Show `refreshed 2s ago` in the header. Coalesce refreshes so a slow `amp` never
queues up behind itself.

### P1-2. The directory picker fights the user

`handle_picker_key` treats `h`/`j`/`k`/`l` as navigation whenever the filter is empty, so a filter can never
**start** with one of those letters. Verified:

- typing `lzv` gives filter `zv`, because `l` browsed into the highlighted entry;
- typing `home` gives filter `ome`, because `h` jumped to the parent directory.

The picker has more problems:

- **The typed text is invisible.** `draw_picker` gives the input `Constraint::Length(3)` and then
  `shrink(chunks[0], 1, 1)`, leaving a 1-row area for a bordered block. Only the top border draws. The
  `use <typed>` row is the only feedback.
- **No scrolling.** `List` is rendered without `ListState`, so in a directory with more subdirectories than
  rows (like `~`), the cursor moves off-screen.
- **`Enter` right after typing picks `use <typed>`** (always row 0), not the best match. Typing `code` +
  `Enter` tries the literal relative path `code`, resolved against lazyamp's process cwd rather than the
  browse root.
- **The browse root isn't shown**, so after `l`/`h` you can't tell where you are.

**Proposed behaviour (fzf / lazygit filter style):**

| Key | Action |
| --- | --- |
| any printable | always goes to the filter |
| `↑`/`↓`, `Ctrl-p`/`Ctrl-n` | move |
| `→` / `Tab` | descend into the highlighted dir |
| `←` / `Backspace` on empty filter | parent |
| `Enter` | pick the highlighted row; `use <typed>` sits at the bottom and is only listed when the typed path exists |
| `Space` | toggle multi-select (see P1-9) |
| `Esc` | close |

Title: ` start runner · ~/code ` (browse root). Render the input as a 1-row `Paragraph` with a block cursor,
with no inner border.

### P1-3. Destructive single keys have no confirmation

`x` (SIGTERM → SIGKILL), `r`, `d`, and `u` all act on one keystroke. Every lazy* tool confirms destructive
actions. Add one reusable `Overlay::Confirm { title, body, on_yes: Cmd }` popup (`y`/`Enter` = yes,
`n`/`Esc` = no) whose body shows the **exact command**:

```
┌ Stop runner ───────────────────────────────────────────┐
│ mac-mini  (pid 48213, up 2h 14m)                       │
│ SIGTERM, then SIGKILL after 2.5 s                      │
│                                                        │
│ Threads running on this runner will be interrupted.    │
│                          [y] stop   [n] cancel         │
└────────────────────────────────────────────────────────┘
```

For `r`, show the replayed command line from P0-3
(`amp --no-tui --runner-id mac-mini --dir ~/code/amp` in `~/code`). That makes the restart semantics
visible and reviewable.

### P1-4. The runner row shows implementation details, not state

The current row: `> dev-box   pid ?   1 dir  [amp]`.

- `[amp]` / `[local]` is the most important signal on screen (listed by Amp, meaning connected, versus a bare
  process), but it's rendered as jargon in the least visible column. At 80 columns it's truncated away
  entirely.
- `pid ?` is noise for the common case. The PID matters only for debugging.
- There is no working directory, uptime, or mode.

**Proposed row** (a `Table` with column widths, not `format!` padding):

```
 ● mac-mini     connected      ~/code          3 dirs   high   2h
 ◐ scratch      starting…      /tmp            1 dir           4s
 ○ lz-review    signed out     ~/rt                             —
```

Color the glyph, not the whole row. Selected row: `Modifier::REVERSED` via `List`/`Table::highlight_style`
plus `ListState`, which also fixes scrolling. Drop the manual `>` marker, which is currently doubled up with
yellow+bold. Move the PID to the detail pane or log header.

### P1-5. Runner logs are written but never shown

`start_runner` sends stdout/stderr to `~/.local/state/lazyamp/logs/<slug>.log`, which is the right call.
But nothing in the UI reads it, and the README only mentions the path in passing. The log pane shows
lazyamp's own actions, not the runner's.

- Add `L` (or `Enter` on a runner): the bottom pane becomes `log · <runner> (<path>)` and tails the file,
  with `f` to follow and `Esc` to go back. Seeing why a runner died is half the reason to open a runner
  manager.
- `fs::File::create` **truncates** the log on every start, so `r` erases the output that explains the crash
  you're restarting from. Open in append mode and write a
  `--- 2026-09-23T06:19:49 start: amp --no-tui … ---` banner, with simple size-based rotation.
- `runner_log_path` falls back to a cwd slug, so two unnamed runners in the same directory share a log file.

### P1-6. Keybinding discoverability

- The hint strip lives in the **title bar** (`draw_title`), is static, and omits `a`, `d`, `g`, `c`, and
  anything pane-specific. The real `amp version` string
  (`0.0.1790142911-g7618fb (released …, 24m ago)`) pushes it off-screen even at 110 columns
  (`… s s`), and at 80 columns it cuts off at `r re`.
- lazygit convention: a **context-sensitive footer**. On the runners pane show
  `s start  x stop  r restart  L logs  a add dir  f defaults  ? help`. On the dirs pane show
  `a add  d remove  y copy path  ? help`. In overlays, show that overlay's keys. Put errors in the same line
  (left side) and keys on the right.
- `?` help doesn't fit at 80×24: `draw_help` wraps long lines and clips the Directories/Other sections,
  with no scroll. Use a two-column layout or make it scrollable.
- Keep **one keymap table** (`const KEYMAP: &[(Context, KeyCode, &str)]`) and generate the help overlay,
  footer, and README table from it. The README table has already drifted from `HELP_TEXT` wording.
- `g` = refresh collides with vim/lazygit muscle memory (`g`/`G` = top/bottom). Use `R` (lazygit) and bind
  `g`/`G` to first/last.

### P1-7. Drop the heuristic text parser; parse `--json` strictly and surface list errors

`parse_runner_text` / `infer_id_from_line` guess at an undocumented table format, and they produce
phantom runners from any prose. With `--json` unavailable, fed through the text fallback:

- `No runners found.` becomes a runner named `No`.
- `A new version of Amp is available…` / `Not logged in: run amp login` become runners `A` and `Not`,
  and `Not` gets the served dir ` run amp login`.

The real CLI supports `--json`, so this heuristic parsing (about 250 lines) buys nothing and risks a lot.

- Define `#[derive(Deserialize)] struct RunnerListJson { runners: Vec<RunnerJson> }` from a **real fixture**
  captured on a signed-in machine, and check it in under `tests/fixtures/`.
- If `--json` fails or doesn't parse, show `Couldn't read amp runner list (amp X.Y). Output: <first line>`
  and ask users to file an issue. Don't guess.
- `AmpClient::list_runners` silently falls back to local-scan-only when `amp runner list` errors, so the
  user never learns that Amp is failing. Return both: `(runners, Option<Warning>)`.
- The error state is weak. `refresh failed: …` is re-logged every 3 s (it filled the log in 10 s), and the
  runners pane still says "No runners on this machine" while the list is failing. Dedupe repeated log
  lines (`… ×12`), keep the last good list dimmed with `(stale · R to retry)`, and render the error in the
  pane when there's no data.

### P1-8. `d` offers to remove dirs that Amp can't remove

Amp's `dirs remove` only removes directories added with `dirs add`. The dirs pane mixes the cwd (inserted
by `runner_from_json` / `merge_runners`), `--dir` args, discovered checkouts, and added dirs with no
distinction, and `remove_selected_dir` fires for all of them.

- Annotate each dir with where it came from: `cwd`, `--dir`, `added`, `discovered`. The cwd and `--dir`
  sources are known locally from the argv.
- Grey out `d` for dirs that can't be removed, with footer copy like
  `cwd can't be removed — restart without it (--no-serve-cwd)`.
- Pane title: ` dirs · mac-mini · serving 3 ` rather than just ` dirs · mac-mini `.

### P1-9. The start flow doesn't match the "one runner, many dirs" model

Amp now recommends a single runner serving many directories
([One Runner Is Now Enough](https://ampcode.com/news/one-runner-is-now-enough)). lazyamp's `s` is still
"pick one cwd, spawn another runner":

- `StartOptions.extra_dirs` exists and is tested, but **no UI path ever sets it**.
- `--discover-dirs=<path>`, `--discover-depth`, `--discover-exclude`, and `--no-serve-cwd` aren't
  exposed.
- There's no per-launch override. To start one runner with `--mode high` you edit global defaults, save,
  start, then edit them back.
- `defaults.runner_id` is a global default, so every `s` reuses the same ID and a second start collides.
  `amp runner dirs --runner-id` can't tell two same-ID runners apart.

**Proposal:**

- `s` opens a small **start form** prefilled from the defaults: working dir (picker), extra dirs
  (multi-select in the picker with `Space`), runner ID (defaulting to the short hostname), mode, and the
  toggles. It shows a live preview line:
  `amp --no-tui --runner-id mac-mini --dir ~/code/amp --dir ~/code/sandcastle`.
  `Enter` starts; `Ctrl-s` starts and saves as defaults.
- If a runner is already running on this machine, `s` first asks
  `mac-mini is already running. Add ~/code/foo to it instead? [a] add  [s] start another  [esc]`.
- Refuse (with an explanation) to start a second runner whose ID matches a running one,
  case-insensitively as Amp does.

### P1-10. Config is rewritten on every quit and state lives in config

- `app_loop` saves unconditionally on quit, and `start_in` / `add_dir` save after every action.
  `toml::to_string_pretty` strips the user's comments and reorders keys. Only write when changed, and use
  `toml_edit` to preserve formatting.
- `recent_dirs` is **state**, not configuration. Move it to `$XDG_STATE_HOME/lazyamp/recent.json`. This
  also deletes the README caveat "Put `recent_dirs` before `[defaults]`…" and the special case in
  `Config::parse`, which exists only because that key lives in a hand-edited file.
- `mode`, `log_level`, and `visibility` are free strings. A typo like `mode = "hgih"` goes straight to
  Amp. Validate on load (warn, keep the value), but allow plugin modes: the real `--mode` help says
  "low, medium, high, ultra, or a plugin mode by key or label".

### P1-11. Module boundaries: split `ui.rs` along state / effects / view

`ui.rs` (1,455 lines) holds the state machine, side effects (spawning, `fs`, signals via `stop_pid`), and
rendering. The `App` methods call `AmpClient` directly, so none of the interaction logic above (restart
semantics, picker keys, confirmation flow) can be unit-tested. That's why the picker `hjkl` bug and the
invisible input shipped with green CI.

`amp.rs` (1,362 lines) mixes four concerns: the CLI client, output parsing, the process table (scan,
signals), and the spawn registry.

There's also a **dependency cycle**: `config` imports `amp::StartOptions` and `amp` imports
`config::state_dir`. `StartDefaults` and `StartOptions` are near-duplicates, and `nonempty` is implemented
three times (`config::nonempty`, `amp::push_opt`'s trim, and `ui::nonempty_owned`).

Suggested layout. This is a rearrangement, not a rewrite; most functions move as-is:

```
src/
  main.rs
  paths.rs          config_path, state_dir, log paths (breaks the cycle)
  config.rs         Config { defaults: StartOptions } (serde on StartOptions, extra_dirs skipped)
  amp/
    mod.rs          trait AmpApi { list, dirs_*, update, version } + AmpClient impl
    args.rs         start_runner_args, dirs_*_args (already pure + tested)
    json.rs         strict serde types for `runner list --json`
  procs.rs          scan, verify_local_amp, stop (signals), argv/cwd readers
  registry.rs       spawned.json with start-time + argv
  app/
    state.rs        App, Overlay, Pane; fn on_key(&mut App, KeyEvent) -> Vec<Cmd>; fn on_msg(&mut App, Msg)
    worker.rs       thread: Cmd -> AmpApi/procs -> Msg
    picker.rs       DirPicker + rebuild + keys (pure, testable)
    flags.rs        FlagField + form
  view/
    mod.rs          draw(frame, &App)
    theme.rs        styles; honour NO_COLOR
    keymap.rs       single table → footer, help, README
```

Tests this unlocks:

- `on_key` sequence tests with a `FakeAmp: AmpApi`. For example, "type `lzv` in the picker → filter ==
  `lzv`", and "`r` on a runner started with `--runner-id alpha` → `Cmd::Start` has argv `[..alpha..]`
  regardless of defaults".
- `ratatui::backend::TestBackend` snapshot tests (`insta`) of `draw` at 80×24 and 120×40, in empty,
  error, loading, and two-runner states.

### P1-12. `install.sh` fails open on checksums

When `checksums.txt` is missing or has no entry for the archive, `verify_checksum` prints a notice and
installs anyway. Every tagged release uploads `checksums.txt`, so for a `curl | sh` installer a missing file
signals a problem. Make it a hard error, with `LAZYAMP_INSECURE_SKIP_VERIFY=1` as the escape hatch.

---

## P2 — polish and taste

- **Version label.** `amp_version_label` only strips `amp `. Show the first token (`0.0.1790142911`) in
  the header and the full string in help/about.
- **Quit copy.** Started runners intentionally outlive lazyamp (`setsid`), which is good, but nothing says
  so. On quit with live runners, print `2 runners still running (mac-mini, scratch). Reopen lazyamp to manage them.`
  to stdout after leaving the alt screen.
- **Status color by substring.** `draw_status` turns the status red if the text contains "fail",
  "not found" or "could not". Store `status: (LogKind, String)` instead; a directory named `failover` is
  currently an "error".
- **Log pane.** Fixed at 7 rows, with no wrapping, so long `amp` errors (stdout + stderr joined on one line
  by `finish_output`) are cut off at the right edge. Truncate with `…`, let `Enter` open the full entry, and
  shrink to 3 rows by default (`+`/`-` or `z` to toggle). Also, `finish_output` produces a double space
  when stdout is empty: `failed (exit status: 1):  unexpected…`.
- **Flags panel.**
  - `h` and `l` both call `nudge_flag(true)`; the `_forward` argument is ignored, so `←` goes forward.
  - `cycle_choice` wraps to `None` after the last value with no visible "(amp default)" slot. Render the
    choices inline, like `mode  [default] low medium ▸high ultra`.
  - `Esc` silently discards unsaved edits. Show `● modified` in the title and confirm on `Esc`, or just
    autosave.
  - The labels are raw flags (`--amp-env`). Add a one-line description footer for the focused field, from
    the Amp docs ("Use Secrets & Env Vars from ampcode.com for threads on this runner").
- **Low-value keys.** `c` (echo config path) and `Enter` on a dir (echo the path into the status line) use
  prime keys for little. Replace them with `e` (open config in `$EDITOR`, reload on return) and `y` (copy
  path via OSC 52) or `o` (open dir in `$EDITOR`).
- **`u` after update.** Runners self-update within about 12 h, and Homebrew installs can't `amp update` at
  all. After a successful update, show which runners are on an older version, and offer
  `restart them now? [y/N]` using the P0-3 replay.
- **Theme.** `Color::DarkGray` carries the key hints and empty states, and it's near-invisible on Solarized
  and some light themes. Prefer `Modifier::DIM` on the default fg, and honour `NO_COLOR`.
- **Empty states.** The copy is good. Make them actionable with the most likely target, like
  `No runners on this machine. s start in ~/code (last used) · ? help`.
- **macOS scan.** `scan_via_ps` splits `command=` on whitespace, so paths with spaces break, and it never
  learns the cwd, so cwd-matching can't work there. Use `ps -o pid=,args=` plus `lsof` for the cwd, or rely
  on `amp runner list --json` as the source of truth.
- **`rust-version`.** The lockfile needs Cargo ≥ 1.85 (edition 2024 dependencies). With distro Cargo
  1.83, `cargo install --path .` fails with a confusing `edition2024` error; I hit this on the review VM. Set
  `rust-version` in `Cargo.toml` and mention it next to `cargo install` in the README.
- **README install target.** "Installs to `~/.local/bin`, or `/usr/local/bin` if writable" reads as if
  `~/.local/bin` is preferred; the script does the reverse. Say "`/usr/local/bin` if writable, otherwise
  `~/.local/bin`".
- **`merge_runners` nit.** `if extra.cwd.is_some() { if let Some(cwd) = &extra.cwd { … } }` can drop the
  outer check.

---

## Keep — what works well

- **Scope discipline.** It manages runners and nothing else: no attempt to reimplement Amp's TUI, threads,
  or orbs. README and `--help` both say so. Keep that line bright.
- **The two-pane model** (runner → its served dirs) is exactly Amp's mental model. The dirs pane title
  naming the selected runner is a nice touch.
- **Pure argv builders** (`start_runner_args`, `dirs_*_args`) with exact-vector tests, plus a stub-Amp
  integration test. That's the right seam; extend it to the whole app (P1-11).
- **`setsid()` on spawn**, so runners survive quitting lazyamp. Correct for a manager of long-lived daemons.
- **Per-runner log files under `$XDG_STATE_HOME`** are the right idea; they just need surfacing (P1-5).
- **The missing-Amp error** (`missing_amp_message`) is exemplary: it says what's missing, why it matters,
  where to get it, how to verify, and gives an override (`AMP_BIN`). Use it as the template for the
  signed-out and parse-failure messages.
- **Hostname validation** of `--runner-id` before save and before start, plus **case-insensitive ID
  matching**, both match Amp's documented semantics.
- **Honest defaults copy**: `(amp default)` vs `(none)` in the flags panel tells the user lazyamp won't pass
  a flag.
- **Refresh pauses while an overlay is open**, so the list never reshuffles under an open picker or form.
- **Panic hook restores the terminal.**
- **Picker seeds** (typed path, cwd, home, recents, then browse) with dedup are the right order; only the
  key handling needs work.
- **Small, boring dependency set** (no async runtime, no clap) and a four-target release with a
  tag/version guard and checksums. The installer itself is careful: `set -eu`, arch aliases, a PATH hint,
  and no API token needed.

---

## Product fit: does this make managing `amp --no-tui` pleasant?

Today it makes **starting** a runner a little easier than typing the command. It doesn't yet make
**running** runners pleasant. You can't tell whether a runner is connected, why it died, or which dirs it
serves and why, and restart quietly changes what's running.

The v0.1 foundations (argv builders, the dirs verbs, the log files) are right, so v0.2 is mostly
surfacing and correctness rather than new plumbing.

### Suggested v0.2 cut, in order

1. **Everything in P0.** Most items are local to one or two functions; P0-5 is a release-matrix change.
2. **Health + logs** (P0-4, P1-4, P1-5). This is the core value of a runner manager.
3. **Async worker + confirmations + footer** (P1-1, P1-3, P1-6). This is what makes it feel like a lazy*
   tool.
4. **Serve-this-dir as the primary verb.** Open lazyamp in a repo and press `a`: the current repo is
   served by this machine's runner, starting one if none is running. That turns lazyamp into the answer to
   "which of my repos are reachable from ampcode.com right now?"
5. **Run at login.** `lazyamp service install` writes a `systemd --user` unit or a `launchd` plist for the
   selected runner's replayed argv. `setsid` children die on reboot; for the stated use case (a spare
   machine that should always be a runner), this is the most-requested feature I'd expect.
6. **Send work to a runner.** `p` on a runner/dir runs
   `amp -x "<prompt>" --executor runner:<id> --runner-dir <dir>` and shows the thread URL. It's a thin
   wrapper over a documented command and closes the loop from inside the TUI.
7. **A non-interactive `lazyamp status [--json]`** that reuses the merged runner model. It's cheap, and it
   lets people script health checks without scraping.

### Explicitly not recommended

- A full TUI rewrite. The layout skeleton (header, two panes, log, status) is fine; the work is in states,
  keys, and correctness.
- More flags in the global defaults panel before per-launch overrides exist (P1-9).
