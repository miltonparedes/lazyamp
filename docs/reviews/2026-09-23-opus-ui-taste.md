# lazyamp v0.1.0 — UI/UX taste & design review

- **Reviewer:** Claude Opus 5.5
- **Date:** 2026-09-23
- **Target:** `main` @ `7fb12b0` (tag `v0.1.0`)
- **Scope:** TUI design, interaction model, UI-layer code design, product polish.
  Deep correctness, Amp CLI parsing races, and release/CI belong to another reviewer and appear here only where they hurt UX.

**Method:** I built the crate with stable Rust (tests and clippy pass), then drove the TUI in tmux at **80×24** and **140×40** against a stub `amp` with ~0.4 s latency per call, which is roughly what the real Node-based CLI costs to start. Every item marked **(observed)** was reproduced this way. Line references point at `7fb12b0`.

---

## 1. Verdict

**This is tidy MVP scaffolding, not yet a lazy\* tool.** The bones are good. The Amp wrapper keeps pure argv builders and parsers separate from the process calls that use them, and both are unit-tested. XDG paths are correct. `setsid` lets runners outlive the TUI. The missing-Amp error is clear. Clippy is clean and the code reads easily. However, the parts a user touches in the first minute don't hold up. The directory picker hides the text you type. None of the lists scroll, so the cursor can walk off-screen and Enter acts on a row you can't see. At 80×24 the help overlay cuts off half its content with no way to scroll. Filtering and pressing Enter runs the literal typed string instead of the match. Every Amp call blocks the event loop, including one every 3 seconds. `x` kills a process with no confirmation. None of these is a deep problem, but lazygit-quality tools are defined by never having this class of rough edge.

The larger gap is structural. lazygit and lazydocker put a list on the left and a rich, context-sensitive main view on the right: logs, diff, details. lazyamp has two flat lists and a 5-line global log. The one thing a runner manager most needs to show (what is this runner doing, and why did it die?) is written to `~/.local/state/lazyamp/logs/*.log` and never shown in the UI. The code mirrors this. `ui.rs` is a single 1,455-line file that mixes state, key handling, blocking I/O, picker logic, and rendering. Overlay state is spread across loose fields, colours are inlined everywhere, and the help text is hand-maintained apart from the real key bindings. **Taste score: about 5/10.** It's competent and honest, and one focused pass on the Must items below would make it feel intentional. The Should items would make it feel like a lazy\* tool.

### What's already right (keep it)

- The two panes (runners, then that runner's directories) are the right primary structure.
- Focus is shown with a cyan border and bold title, the unfocused state is dimmed, and the selection is yellow. That's a coherent base palette.
- The empty-state copy is useful: "No runners on this machine. Press s to start".
- The picker content model is right: typed path, then cwd, home, recents, then browse.
- Failures print a clear message and exit non-zero, with no panics. The panic hook restores the terminal.
- `amp.rs` exposes pure `*_args` builders and `parse_*` functions. That's the right seam and it's tested.

---

## 2. Must (fix before calling it lazy\*-quality)

Everything in this section is observed, user-visible, and cheap relative to its impact.

### M1. Lists don't scroll; the selection can be off-screen (observed)
All three lists render `List::new(items)` with a hand-drawn `>` marker and no `ListState` (`ui.rs:1186`, `1217`, `1381`). ratatui only scrolls a list that is given a state to track. In a directory with 40 subfolders I pressed Down 25 times: the highlight disappeared off the bottom, and Enter started a runner in `proj-23`, which was never visible.
**Fix:** keep a `ListState` for each list, render with `render_stateful_widget`, and use `.highlight_symbol("▶ ")` plus `.highlight_style(...)` instead of formatting the marker by hand. Add a `Scrollbar` when items exceed the height.

### M2. The picker's filter text is invisible (observed)
The input row gets `Constraint::Length(3)` from the *outer* popup and is then `shrink(…, 1, 1)`ed down to 1 row (`ui.rs:1346–1366`), so only the input block's top border draws. The only hint of what you typed is the "use cod" entry.
**Fix:** lay out inside `outer.inner(popup)`, give the input 3 rows unshrunk, and place a real terminal cursor with `frame.set_cursor_position` rather than appending `_`.

### M3. Picker key model traps users (observed)
- With an empty filter, `h`, `j`, `k`, and `l` navigate (`ui.rs:382–410`). That means you **can't start a filter with h, j, k, or l**. Typing `lazyamp` begins by descending into whichever entry is selected.
- The literal typed text is always the first entry and is never filtered out (`ui.rs:855–861`, `902–908`). So "type to fuzzy-filter, press Enter" selects the raw string. Typing `cod` and pressing Enter gave `not a directory: cod`, even though `~/code` was the next row.
- The directory being browsed is never shown, so after `←` you don't know where you are. `..` also disappears when the parent equals home, because it gets deduplicated.

**Fix:** make the picker always accept text, like fzf or telescope. Letters filter; `↑`/`↓` and `Ctrl-n`/`Ctrl-p` move; `Tab` or `→` descends; `←`, or Backspace on an empty filter, goes to the parent; Enter selects the **highlighted** row. Only offer "use <typed>" when the input looks like a path (starts with `/`, `~`, or `.`), and rank it *below* real matches unless the path exists. Show the current browse directory as a breadcrumb in the input title, e.g. `filter · ~/code`.

### M4. The event loop blocks on every Amp call (observed)
`refresh()` runs `amp runner list --json` (sometimes followed by plain `amp runner list`) plus a `/proc` scan, synchronously, every 3 s (`ui.rs:245`). `stop_pid` sleeps up to 2.5 s on the UI thread (`amp.rs:844–851`). `amp update` freezes the UI for as long as it takes (`ui.rs:230–233`). Startup enters raw mode and the alternate screen *before* two blocking Amp calls (`ui.rs:206–219`), so first launch shows a blank screen. With the real CLI, keystrokes will stutter every 3 s.
**Fix:** add a small job runner, one worker thread with an `mpsc` channel. Each UI action sends a `Job` (`ListRunners`, `Stop(pid)`, `Update`, and so on) and gets back an `AppEvent::JobDone(result)`. Draw the first frame immediately with a "loading…" state. Show a spinner in the status line and per-row states such as `stopping…`, and prevent overlapping refreshes.

### M5. Destructive actions have no confirmation, and stop can report false success (observed)
`x` sends SIGTERM then SIGKILL immediately. `d` removes a served directory immediately. `r` kills the runner and restarts it with *the current saved defaults* rather than the runner's original flags (`ui.rs:685–690`), which silently changes its configuration. Separately, `x` on a PID that no longer exists logged **"stopped … (pid 4242)"**, because `send_signal` ignores `ESRCH` (`amp.rs:871–876`).
**Fix:** add a small reusable `Confirm` modal: `Stop milton-workstation (pid 4242)? [y/N]`. Also have `r` show a one-line diff when the restart flags differ from the running ones. Report "already exited" when the first signal returns `ESRCH`.

### M6. The help overlay is clipped at 80×24 and can't scroll (observed)
The help popup is 80 % of the height with wrapping (`ui.rs:1267–1279`). At 80×24 it shows only through `g refresh`; the Directories and Other sections are never visible. Wrapping also breaks the key and description columns, e.g. "Enter confirm … typed\npath".
**Fix:** generate help from a keymap table (see S1) and render it as a two-column `Table`. Size the popup `min(area, 76×N)` rather than by percentage, allow `j`/`k` scrolling with a scrollbar, and group bindings by context.

### M7. The status bar is overwritten and its severity is guessed from text (observed)
`log()` writes to both the log and the status line (`ui.rs:839`). The next auto-refresh replaces the status with `"2 runner(s)"` (`ui.rs:804`), so errors flash for under 3 s. Status colour comes from searching the text for "fail", "not found", or "could not" (`ui.rs:1253–1256`), so `could not determine PID` is red but `refusing to start: …` and `not a directory: …` are grey.
**Fix:** keep the status as a *toast* with a `Severity` and a TTL (errors sticky until the next key press). Refresh should update a separate "N runners · refreshed 2s ago" segment instead of the toast. Never infer colour from strings.

---

## 3. Should (what makes it feel like a lazy\* tool)

### Interaction and layout

**S1. One keymap as the source of truth.** Add an `Action` enum and a `const KEYMAP: &[(KeyContext, KeyCode, Action, &str)]`. The event loop, help overlay, footer, and README table can all be derived from it, and a test can check the README against it. Today the key lists in `HELP_TEXT` (`ui.rs:28–54`), the header hints (`ui.rs:1117`), and the README have already drifted: the header omits `a`, `d`, and `g`, and at 80 columns it's truncated after `r restart`.

**S2. A context-sensitive footer instead of header hints.** Like lazygit, keep a bottom line listing the keys valid *for the focused pane or modal*, with `?` for the rest. Use the title bar for identity and state: `lazyamp 0.1.0 · amp 0.0.1234 · 2 runners · ⟳ 2s`.

**S3. The right side should show detail and logs, not a second flat list.** Replace the "dirs" pane with a tabbed detail view for the selected runner: **Dirs | Logs | Info**.
- *Logs* tails `state_dir()/logs/<slug>.log`, which lazyamp already writes but never shows. Add follow mode, `G` to jump to the end, and wrap toggled with `w`. This is the most important missing affordance: a runner that dies at start currently shows only "started … (pid N)" and then disappears.
- *Info* shows the ID, PID, source (amp list / local scan / spawned by lazyamp), cwd, the resolved argv, the log path, and uptime if available.

**S4. Richer runner rows with status glyphs.** Use `● running`, `○ no PID`, and `◌ stopping…` in place of `pid ?`. Replace `[amp]`/`[local]` with a glyph or dim suffix explained in the Info tab. Columns should adapt to the width rather than a fixed `{:<20} {:<12}` (`ui.rs:1174–1178`); at 80 columns the directory count and source are clipped off (observed). Truncate the *middle* of long paths (`~/code/very/…/long/name`) instead of cutting the end with no ellipsis (observed in the dirs pane).

**S5. A launch sheet between the picker and the spawn.** Today `s` goes straight from the picker to `amp --no-tui` with whatever defaults are saved, so a one-off `--mode high` means editing persistent config. After choosing a directory, show a compact sheet: the resolved command line, `Enter` to launch, `e` to tweak flags *for this launch only*, and `Ctrl-s` to also save those flags as defaults. Also add `S` = "start in the current directory, no questions".

**S6. Flags panel polish.**
- `h`/`←` also cycles *forward* (`ui.rs:358`, and `nudge_flag` ignores its `_forward` argument). Make it cycle backward.
- `q` closes and silently discards edits (`ui.rs:352`). Show a dirty marker (`flags *`) and ask `discard changes? y/N`.
- Add a one-line description of each flag and a live preview of `amp --no-tui …` at the bottom.
- `Backspace` or `x` on a row resets it to the Amp default.
- Validate runner IDs *inline* with a red hint on the row, not only in the log.
- The instruction line is truncated at 80 columns (observed). Put hints in the footer (S2).
- `saved at <path>` reads as "was saved", even before saving. Label it `config: <path>`.

**S7. Fix empty and first-run states.**
- With no runners, the dirs pane says "Press a to add", but `a` then errors with "select a runner before adding a directory" (`ui.rs:565–568`, `1199`). That's a dead end. It should say "Start a runner first (s)".
- Center the empty-state message in the pane and point to the primary action: `s start in a directory · S start here`.
- On first run (no config file), show a one-time dismissible hint: `?` help · `f` defaults · config path.

**S8. Consistent verbs and lazy\*-standard keys.**
- In vim-flavoured TUIs, `g` means "top" (with `G` for bottom), not refresh. Use `R` for refresh, as lazygit does, and `g`/`G` for top and bottom.
- `c` (show the config path in the status bar) is low value. Use `e` to open the config in `$EDITOR`, with suspend/resume of the TUI.
- `Enter` on a directory only echoes its path. Make Enter focus or open detail, `o` open the directory in `$EDITOR`, and `y` copy the path (OSC 52).
- Add `1`/`2`/`3` to jump straight to a pane, and make `Shift-Tab` cycle in reverse. Today it's an alias for `Tab`.

**S9. Popup sizing and resize safety.** `centered()` is percentage-based (`ui.rs:1390–1408`), so popups are cramped at 80×24 and oversized at 200×60. Use fixed preferred sizes clamped to the terminal. Add a minimum-size guard ("enlarge terminal to 60×16") rather than rendering clipped junk.

**S10. Make the log pane a real pane.** It's fixed at 7 rows, has no wrap and no scroll, and can't take focus (`ui.rs:1220–1250`). Long errors, such as the PID-detection message at `ui.rs:650`, get truncated. Let it take focus with `3`, scroll, wrap, and use `+`/`-` to grow or shrink it (lazygit's `+`/`_`). Prefix entries with `✓`/`✗`/`·` so severity survives `NO_COLOR`.

### Code design (UI layer ↔ Amp client)

**S11. Split `ui.rs` along responsibilities.** Suggested layout:

```
src/
  app/
    mod.rs        # App state + `fn update(&mut self, Action) -> Vec<Job>`
    action.rs     # Action enum, KEYMAP (S1)
    modal.rs      # Modal enum and per-modal state (see S12)
    jobs.rs       # worker thread, Job / JobResult (M4)
  view/
    mod.rs        # draw(frame, &App), layout, min-size guard
    runners.rs  detail.rs  log.rs  footer.rs  help.rs  flags.rs  picker.rs  confirm.rs
    theme.rs      # all colours/styles, NO_COLOR handling
  picker.rs       # pure model: entries, ranking, browse; no ratatui, fully unit-tested
  paths.rs        # expand_path / display_path / middle-truncate
  amp.rs  config.rs
```

**S12. Make invalid UI states unrepresentable.** `Overlay` is a bare enum (`ui.rs:62–69`), while its data lives in loose `App` fields: `picker: Option<DirPicker>`, `flag_field`, `edit_buffer`, and `flags` (`ui.rs:167–185`). The picker handler even has to recover from `Overlay::Picker` with `picker == None` (`ui.rs:364–367`). Use enums that carry their data:

```rust
enum Modal {
    None,
    Help { scroll: u16 },
    Flags(FlagsForm),                 // draft + cursor + dirty + Option<EditField>
    Picker(DirPicker),
    Confirm(ConfirmDialog),           // prompt + Action to run on "y"
    LaunchSheet(LaunchSheet),
}
```

**S13. Put the Amp client behind a trait so the UI is testable.** `App` owns a concrete `AmpClient` and calls it synchronously. Add a trait (`trait Amp { fn list_runners(&self) -> Result<Vec<Runner>>; … }`) with an in-memory fake. Then add ratatui `TestBackend` snapshot tests (with `insta`) for the empty state, the 80×24 layout, the picker with the filter text visible, and the help overlay. M1, M2, and M6 would each have been caught by a single snapshot. Today the UI tests cover only `fuzzy_match`, `expand_path`, and `amp_version_label`.

**S14. Tighten the domain types.**
- `Runner { id: String, from_amp_list: bool, from_local_scan: bool }` (`amp.rs:30–37`): an empty `id` means "unnamed", and the two booleans encode a set. Prefer `id: Option<RunnerId>` and `source: RunnerSource` (or an `enumflags`-style set), plus a `status` field.
- `StartOptions` (`amp.rs:15–26`) and `StartDefaults` (`config.rs:24–46`) are near-duplicates kept in sync by hand. Make `StartDefaults` a `#[serde]` wrapper around `StartOptions`, or the reverse.
- `mode`, `log_level`, and `visibility` are `Option<String>`, with choices in `&[&str]` constants (`config.rs:200–202`) and `cycle_choice` doing case-insensitive lookups. Use small `enum`s with `Display`, `FromStr`, and `ALL`. The flags panel then gets per-value descriptions and correct next/previous behaviour for free.

**S15. Centralise the theme.** `Color::Cyan`, `Color::Yellow`, and `Color::DarkGray` are scattered through about 15 call sites. Move them to a `Theme` struct with semantic slots (`focus_border`, `selection`, `muted`, `ok`, `err`), honour `NO_COLOR`, and leave room for a `[theme]` section in the config later.

---

## 4. Nice (v0.2 and later)

- **Mouse support:** click to focus a pane or row, and scroll wheel in lists and logs (crossterm `EnableMouseCapture`).
- **`/` to filter the runner and dir lists**, the same as the picker.
- **Picker extras:** `.` to toggle hidden dirs (they're always skipped today), a marker on git repos, and zoxide/`z` integration for recents.
- **Toast queue** with fade-out, rather than a single status string.
- **`amp update` as a modal job** with streamed output and a "restart runners to pick up the new version?" follow-up.
- **Remembered layout state** (focused pane, log height) across launches.
- **Command palette** (`:` or `Ctrl-p`) listing every `Action` from the keymap.
- **Per-runner quick actions:** "open Amp TUI here" (`amp` in the runner's cwd via suspend/resume) and "copy runner ID".

---

## 5. Proposed layout and keymap

```
 lazyamp 0.1.0 · amp 0.0.1234 · 2 runners                          ⟳ 2s ago
┌[1] Runners ───────────────────────┐┌[2] Dirs │ Logs │ Info ─────────────────┐
│▶ ● milton-workstation  4242    3  ││  ~/code/lazyamp                        │
│  ○ garage-box          —       1  ││  ~/code/amp-dotfiles                   │
│                                   ││  ~/code/very/…/with/a/long/name        │
│                                   ││                                        │
│                                   ││                                        │
└───────────────────────────────────┘└────────────────────────────────────────┘
┌[3] Activity ────────────────────────────────────────────────────────────────┐
│06:15:08 ✓ stopped milton-workstation (pid 4242)                             │
│06:15:07 ✗ not a directory: ~/cod                                            │
└─────────────────────────────────────────────────────────────────────────────┘
 s start  S start here  x stop  r restart  f flags  [ ] tabs  ? help   ⠋ refreshing
```

| Context | Key | Action |
| --- | --- | --- |
| Global | `?` | Help (scrollable, generated from keymap) |
| Global | `q` / `Ctrl-c` | Quit |
| Global | `Tab` / `Shift-Tab`, `1` `2` `3` | Cycle / jump pane |
| Global | `j` `k` / `↑` `↓`, `g` `G` | Move, top / bottom |
| Global | `R` | Refresh now |
| Global | `f` | Start defaults (flags) |
| Global | `e` | Edit config in `$EDITOR` |
| Global | `u` | `amp update` (confirm, runs in background) |
| Runners | `s` / `S` | Start (picker → launch sheet) / start in cwd |
| Runners | `x` | Stop (confirm) |
| Runners | `r` | Restart (confirm, shows flag diff) |
| Runners | `Enter` | Focus detail |
| Detail | `[` / `]` | Previous / next tab (Dirs, Logs, Info) |
| Dirs tab | `a` / `d` | Add / remove dir (confirm) |
| Dirs tab | `o` / `y` | Open in `$EDITOR` / copy path |
| Logs tab | `G`, `w` | Follow tail, toggle wrap |
| Picker | type, `↑` `↓`, `Tab`/`→`, `←`, `Enter`, `Esc` | Filter, move, descend, parent, select highlighted, cancel |

---

## 6. Quick wins (trivial, safe, suggested but not applied)

1. **Flags `h`/`←` should cycle backward** (`ui.rs:358`). Pass `false` and add a `cycle_choice_prev`.
2. **Show the picker input:** lay out inside `outer.inner(popup)` and stop shrinking the 3-row input (`ui.rs:1346–1366`).
3. **Use `ListState` + `highlight_symbol`** for the runners, dirs, and picker lists (`ui.rs:1162–1186`, `1205–1217`, `1368–1382`).
4. **Header hints:** add `a add  d remove  g refresh`, or better, move them to a footer (S2).
5. **Flags header copy:** replace "Start defaults (Enter/space to edit or cycle, s save, esc cancel)" with "enter edit · s save · esc cancel", which fits at 80 columns.
6. **`saved at <path>` → `config: <path>`** in the flags panel (`ui.rs:1310`).
7. **Dirs empty state without a runner:** "Start a runner first (s)" instead of "Press a to add" (`ui.rs:1199`).
8. **`pid ?` → `—`** (`ui.rs:1171`), and add a small legend or drop the `[amp]`/`[local]` badge.
9. **Help title:** `" help  (esc) "` → `" Help · esc close "`. Also make `Shift-Tab` reverse the cycle (`ui.rs:313`).
10. **Missing-Amp message** always says "not found on PATH", even when `AMP_BIN` is set (`amp.rs:977–986`). Say "`AMP_BIN` points to `…`, which is not an executable file" in that case.
11. **README:** move "developed against public contracts (Amp was not installed on the agent VM)" (`README.md:13`) and the "Releasing" section into `CONTRIBUTING.md`. A user README shouldn't undermine confidence or document maintainer steps. Lead with a screenshot or GIF, as lazy\* READMEs do.
12. **README config note** "Put `recent_dirs` before `[defaults]`" (`README.md:102`) exposes an implementation quirk. lazyamp already writes `recent_dirs` first, and `Config::parse` accepts either placement, so the note can simply go.
13. **Set `rust-version` in `Cargo.toml`:** `cargo install --path .` fails on Rust 1.83 with an `edition2024` error from a transitive dependency (observed). A declared MSRV turns that into a clear message.

---

## 7. Adjacent issues that hurt UX (flagged for the correctness reviewer)

- **Config clobbering:** `Config::load().unwrap_or_default()` (`ui.rs:203`) silently ignores a parse error, and `app_loop` saves on quit (`ui.rs:249`). A single typo in `config.toml` therefore **wipes the user's defaults and recents** on the next exit. At minimum, show the parse error and don't save until the config has loaded cleanly.
- **False success on start:** `start_runner` returns as soon as `spawn()` succeeds. If `amp` exits immediately (bad flag, auth), the UI still says "started … (pid N)". Check liveness after about 500 ms and show the tail of the log on failure (pairs with S3).
- **Log truncation:** `File::create` truncates the runner log on every start or restart (`amp.rs:150`), destroying the crash output you'd want to read after a failed restart. Append with a separator, or rotate.
- **Non-atomic restart:** if the stop succeeds and the spawn fails, the runner stays down and the user only sees "restart spawn failed".
