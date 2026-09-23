//! Keyboard-first runner manager (ratatui + crossterm).

use crate::amp::{is_plausible_runner_id, stop_pid, AmpClient, Runner, StartOptions};
use crate::config::{
    config_path, cycle_choice, Config, StartDefaults, LOG_LEVELS, MODES, VISIBILITIES,
};
use anyhow::Result;
use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REFRESH_EVERY: Duration = Duration::from_secs(3);
const LOG_CAP: usize = 200;
const HELP_TEXT: &str = "\
lazyamp — manage Amp --no-tui runners

Navigation
  hjkl / arrows     move; h/l also switch panes
  Tab               next pane (runners ↔ dirs)
  Enter             confirm (picker / flags / typed path)
  Esc               close overlay
  q / Ctrl-c        quit
  ?                 this help

Runners
  s                 start amp --no-tui in a chosen directory
  x                 stop the selected runner (SIGTERM, then SIGKILL)
  r                 restart selected runner
  g                 refresh `amp runner list`

Directories
  a                 add a served directory (`amp runner dirs add`)
  d                 remove the selected directory (`amp runner dirs remove`)

Other
  f                 common flags / defaults (saved to config)
  u                 run `amp update`
  c                 show config path

This UI does not open Amp's interactive agent TUI.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Runners,
    Dirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Flags,
    Picker,
    EditField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerPurpose {
    Start,
    AddDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagField {
    RunnerId,
    Mode,
    LogLevel,
    Visibility,
    SettingsFile,
    McpConfig,
    RemoteControl,
    DiscoverDirs,
    AmpEnv,
}

impl FlagField {
    const ALL: [FlagField; 9] = [
        FlagField::RunnerId,
        FlagField::Mode,
        FlagField::LogLevel,
        FlagField::Visibility,
        FlagField::SettingsFile,
        FlagField::McpConfig,
        FlagField::RemoteControl,
        FlagField::DiscoverDirs,
        FlagField::AmpEnv,
    ];

    fn next(self) -> Self {
        let i = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    fn is_text(self) -> bool {
        matches!(
            self,
            FlagField::RunnerId | FlagField::SettingsFile | FlagField::McpConfig
        )
    }

    fn is_cycle(self) -> bool {
        matches!(
            self,
            FlagField::Mode | FlagField::LogLevel | FlagField::Visibility
        )
    }

    fn label(self) -> &'static str {
        match self {
            FlagField::RunnerId => "--runner-id",
            FlagField::Mode => "--mode",
            FlagField::LogLevel => "--log-level",
            FlagField::Visibility => "--visibility",
            FlagField::SettingsFile => "--settings-file",
            FlagField::McpConfig => "--mcp-config",
            FlagField::RemoteControl => "--remote-control-terminal",
            FlagField::DiscoverDirs => "--discover-dirs",
            FlagField::AmpEnv => "--amp-env",
        }
    }
}

#[derive(Debug, Clone)]
struct PickerEntry {
    label: String,
    path: PathBuf,
    kind: PickerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Typed,
    Special,
    Recent,
    Browse,
}

#[derive(Debug, Clone)]
struct DirPicker {
    purpose: PickerPurpose,
    input: String,
    browse_root: PathBuf,
    cursor: usize,
    entries: Vec<PickerEntry>,
}

struct App {
    client: AmpClient,
    config: Config,
    config_path: PathBuf,
    runners: Vec<Runner>,
    selected_runner: usize,
    selected_dir: usize,
    pane: Pane,
    overlay: Overlay,
    picker: Option<DirPicker>,
    flags: StartDefaults,
    flag_field: FlagField,
    edit_buffer: String,
    log: VecDeque<LogLine>,
    status: String,
    amp_version: String,
    last_refresh: Instant,
    should_quit: bool,
    pending_update: bool,
}

struct LogLine {
    time: String,
    text: String,
    kind: LogKind,
}

#[derive(Clone, Copy)]
enum LogKind {
    Info,
    Ok,
    Error,
}

/// Run the TUI. Restores the terminal on exit or panic.
pub fn run() -> Result<()> {
    let client = AmpClient::detect()?;
    let config = Config::load().unwrap_or_default();
    let config_path = config_path();

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    let mut app = App::new(client, config, config_path);
    let result = app_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn app_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        if app.pending_update {
            terminal.draw(|f| draw(f, app))?;
            app.run_update();
        }
        terminal.draw(|f| draw(f, app))?;
        if app.should_quit {
            break;
        }
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key);
                }
            }
        }
        if app.overlay == Overlay::None && app.last_refresh.elapsed() >= REFRESH_EVERY {
            app.refresh();
        }
    }
    let _ = app.config.save_to(&app.config_path);
    Ok(())
}

impl App {
    fn new(client: AmpClient, config: Config, config_path: PathBuf) -> Self {
        let flags = config.defaults.clone();
        let amp_version = client.version().unwrap_or_else(|_| "unknown".into());
        let mut app = Self {
            client,
            config,
            config_path,
            runners: Vec::new(),
            selected_runner: 0,
            selected_dir: 0,
            pane: Pane::Runners,
            overlay: Overlay::None,
            picker: None,
            flags,
            flag_field: FlagField::RunnerId,
            edit_buffer: String::new(),
            log: VecDeque::new(),
            status: "ready".into(),
            amp_version,
            last_refresh: Instant::now() - REFRESH_EVERY,
            should_quit: false,
            pending_update: false,
        };
        app.log(
            LogKind::Info,
            format!(
                "Amp {} — {}",
                amp_version_label(&app.amp_version),
                app.client.binary.display()
            ),
        );
        app.refresh();
        app
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.overlay {
            Overlay::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => {
                    self.overlay = Overlay::None;
                }
                _ => {}
            },
            Overlay::Flags => self.handle_flags_key(key),
            Overlay::Picker => self.handle_picker_key(key),
            Overlay::EditField => self.handle_edit_key(key),
            Overlay::None => self.handle_normal_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            KeyCode::Tab => self.cycle_pane(),
            KeyCode::BackTab => self.cycle_pane(),
            KeyCode::Char('h') | KeyCode::Left => self.pane = Pane::Runners,
            KeyCode::Char('l') | KeyCode::Right => self.pane = Pane::Dirs,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('g') => self.refresh(),
            KeyCode::Char('s') => self.open_picker(PickerPurpose::Start),
            KeyCode::Char('x') => self.stop_selected(),
            KeyCode::Char('r') => self.restart_selected(),
            KeyCode::Char('a') => self.open_picker(PickerPurpose::AddDir),
            KeyCode::Char('d') => self.remove_selected_dir(),
            KeyCode::Char('u') => {
                self.status = "Running `amp update`…".into();
                self.pending_update = true;
            }
            KeyCode::Char('f') => {
                self.flags = self.config.defaults.clone();
                self.flag_field = FlagField::RunnerId;
                self.overlay = Overlay::Flags;
            }
            KeyCode::Char('c') => {
                self.log(
                    LogKind::Info,
                    format!("config {}", self.config_path.display()),
                );
                self.status = self.config_path.display().to_string();
            }
            KeyCode::Enter if self.pane == Pane::Dirs => {
                if let Some(dir) = self.selected_dir_path() {
                    self.status = dir.display().to_string();
                }
            }
            _ => {}
        }
    }

    fn handle_flags_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::None,
            KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Char('j') | KeyCode::Down => self.flag_field = self.flag_field.next(),
            KeyCode::Char('k') | KeyCode::Up => self.flag_field = self.flag_field.prev(),
            KeyCode::Char('s') => self.save_flags(),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_flag_field(),
            KeyCode::Char('l') | KeyCode::Right => self.nudge_flag(true),
            KeyCode::Char('h') | KeyCode::Left => self.nudge_flag(true),
            _ => {}
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            self.overlay = Overlay::None;
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                self.overlay = Overlay::None;
            }
            KeyCode::Enter => {
                if let Some(entry) = picker.entries.get(picker.cursor).cloned() {
                    let path = entry.path;
                    let purpose = picker.purpose;
                    self.picker = None;
                    self.overlay = Overlay::None;
                    self.finish_picker(purpose, path);
                }
            }
            KeyCode::Char('l') | KeyCode::Right
                if key.modifiers.is_empty() && picker.input.is_empty() =>
            {
                if let Some(entry) = picker.entries.get(picker.cursor).cloned() {
                    if entry.path.is_dir() {
                        picker.browse_root = entry.path;
                        picker.input.clear();
                        picker.cursor = 0;
                        rebuild_picker(picker, &self.config);
                    }
                }
            }
            KeyCode::Char('h') | KeyCode::Left
                if key.modifiers.is_empty() && picker.input.is_empty() =>
            {
                if let Some(parent) = picker.browse_root.parent() {
                    picker.browse_root = parent.to_path_buf();
                    picker.cursor = 0;
                    rebuild_picker(picker, &self.config);
                }
            }
            KeyCode::Down | KeyCode::Char('j') if picker.input.is_empty() => {
                if picker.cursor + 1 < picker.entries.len() {
                    picker.cursor += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') if picker.input.is_empty() => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Down => {
                if picker.cursor + 1 < picker.entries.len() {
                    picker.cursor += 1;
                }
            }
            KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Backspace => {
                picker.input.pop();
                rebuild_picker(picker, &self.config);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.input.push(c);
                rebuild_picker(picker, &self.config);
            }
            _ => {}
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.overlay = Overlay::Flags;
            }
            KeyCode::Enter => {
                self.commit_edit();
                self.overlay = Overlay::Flags;
            }
            KeyCode::Backspace => {
                self.edit_buffer.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit_buffer.push(c);
            }
            _ => {}
        }
    }

    fn activate_flag_field(&mut self) {
        if self.flag_field.is_text() {
            self.edit_buffer = flag_text(&self.flags, self.flag_field).unwrap_or_default();
            self.overlay = Overlay::EditField;
        } else if self.flag_field.is_cycle() {
            self.nudge_flag(true);
        } else {
            match self.flag_field {
                FlagField::RemoteControl => {
                    self.flags.remote_control_terminal = !self.flags.remote_control_terminal;
                }
                FlagField::DiscoverDirs => self.flags.discover_dirs = !self.flags.discover_dirs,
                FlagField::AmpEnv => self.flags.amp_env = !self.flags.amp_env,
                _ => {}
            }
        }
    }

    fn nudge_flag(&mut self, _forward: bool) {
        match self.flag_field {
            FlagField::Mode => {
                self.flags.mode = cycle_choice(self.flags.mode.as_deref(), MODES);
            }
            FlagField::LogLevel => {
                self.flags.log_level = cycle_choice(self.flags.log_level.as_deref(), LOG_LEVELS);
            }
            FlagField::Visibility => {
                self.flags.visibility =
                    cycle_choice(self.flags.visibility.as_deref(), VISIBILITIES);
            }
            FlagField::RemoteControl => {
                self.flags.remote_control_terminal = !self.flags.remote_control_terminal;
            }
            FlagField::DiscoverDirs => self.flags.discover_dirs = !self.flags.discover_dirs,
            FlagField::AmpEnv => self.flags.amp_env = !self.flags.amp_env,
            _ => {}
        }
    }

    fn commit_edit(&mut self) {
        let value = nonempty_owned(&self.edit_buffer);
        match self.flag_field {
            FlagField::RunnerId => self.flags.runner_id = value,
            FlagField::SettingsFile => self.flags.settings_file = value,
            FlagField::McpConfig => self.flags.mcp_config = value,
            _ => {}
        }
    }

    fn save_flags(&mut self) {
        if let Some(id) = self.flags.runner_id.as_deref() {
            if !is_plausible_runner_id(id) {
                self.log(
                    LogKind::Error,
                    format!("runner-id `{id}` is not a hostname (letters, digits, '-', '.')"),
                );
                return;
            }
        }
        self.config.defaults = self.flags.clone();
        match self.config.save_to(&self.config_path) {
            Ok(()) => {
                self.log(
                    LogKind::Ok,
                    format!("saved defaults to {}", self.config_path.display()),
                );
                self.overlay = Overlay::None;
            }
            Err(err) => self.log(LogKind::Error, format!("save failed: {err:#}")),
        }
    }

    fn cycle_pane(&mut self) {
        self.pane = match self.pane {
            Pane::Runners => Pane::Dirs,
            Pane::Dirs => Pane::Runners,
        };
    }

    fn move_sel(&mut self, delta: i32) {
        match self.pane {
            Pane::Runners => {
                if self.runners.is_empty() {
                    return;
                }
                let next = self.selected_runner as i32 + delta;
                self.selected_runner = next.clamp(0, self.runners.len() as i32 - 1) as usize;
                self.selected_dir = 0;
            }
            Pane::Dirs => {
                let n = self.current_dirs().len();
                if n == 0 {
                    return;
                }
                let next = self.selected_dir as i32 + delta;
                self.selected_dir = next.clamp(0, n as i32 - 1) as usize;
            }
        }
    }

    fn current_runner(&self) -> Option<&Runner> {
        self.runners.get(self.selected_runner)
    }

    fn current_dirs(&self) -> Vec<PathBuf> {
        self.current_runner()
            .map(|r| r.dirs.clone())
            .unwrap_or_default()
    }

    fn selected_dir_path(&self) -> Option<PathBuf> {
        self.current_dirs().get(self.selected_dir).cloned()
    }

    fn open_picker(&mut self, purpose: PickerPurpose) {
        if purpose == PickerPurpose::AddDir && self.current_runner().is_none() {
            self.log(LogKind::Error, "select a runner before adding a directory");
            return;
        }
        let browse_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut picker = DirPicker {
            purpose,
            input: String::new(),
            browse_root,
            cursor: 0,
            entries: Vec::new(),
        };
        rebuild_picker(&mut picker, &self.config);
        self.picker = Some(picker);
        self.overlay = Overlay::Picker;
    }

    fn finish_picker(&mut self, purpose: PickerPurpose, path: PathBuf) {
        let path = expand_path(&path.display().to_string());
        if !path.is_dir() {
            self.log(
                LogKind::Error,
                format!("not a directory: {}", path.display()),
            );
            return;
        }
        match purpose {
            PickerPurpose::Start => self.start_in(&path),
            PickerPurpose::AddDir => self.add_dir(&path),
        }
    }

    fn start_options(&self) -> StartOptions {
        self.config.defaults.to_start_options()
    }

    fn start_in(&mut self, cwd: &Path) {
        if let Some(id) = self.config.defaults.runner_id.as_deref() {
            if !is_plausible_runner_id(id) {
                self.log(
                    LogKind::Error,
                    format!("refusing to start: runner-id `{id}` is not a hostname"),
                );
                return;
            }
        }
        let opts = self.start_options();
        match self.client.start_runner(cwd, &opts) {
            Ok(pid) => {
                self.config.remember_dir(cwd);
                let _ = self.config.save_to(&self.config_path);
                let id = opts
                    .runner_id
                    .clone()
                    .unwrap_or_else(|| "(amp-assigned id)".into());
                self.log(
                    LogKind::Ok,
                    format!("started {id} in {} (pid {pid})", display_path(cwd)),
                );
                self.status = format!("started pid {pid}");
                self.refresh();
            }
            Err(err) => self.log(LogKind::Error, format!("start failed: {err:#}")),
        }
    }

    fn stop_selected(&mut self) {
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        match resolve_stop_pid(&runner) {
            Some(pid) => match stop_pid(pid) {
                Ok(()) => {
                    self.log(
                        LogKind::Ok,
                        format!("stopped {} (pid {pid})", runner.display_id()),
                    );
                    self.refresh();
                }
                Err(err) => self.log(LogKind::Error, format!("stop failed: {err:#}")),
            },
            None => self.log(
                LogKind::Error,
                format!(
                    "could not determine PID for {}. `amp runner list` had no PID and no matching amp --no-tui process was found.",
                    runner.display_id()
                ),
            ),
        }
    }

    fn restart_selected(&mut self) {
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        let cwd = runner.cwd.clone().or_else(|| runner.dirs.first().cloned());
        let Some(cwd) = cwd else {
            self.log(
                LogKind::Error,
                "cannot restart: no working directory known for this runner",
            );
            return;
        };
        if let Some(pid) = resolve_stop_pid(&runner) {
            if let Err(err) = stop_pid(pid) {
                self.log(
                    LogKind::Error,
                    format!("stop before restart failed: {err:#}"),
                );
                return;
            }
        } else {
            self.log(
                LogKind::Error,
                "cannot restart: no PID (see README for PID detection)",
            );
            return;
        }
        let mut opts = self.start_options();
        if opts.runner_id.is_none() {
            if let Some(id) = runner.runner_id_flag() {
                opts.runner_id = Some(id.to_string());
            }
        }
        match self.client.start_runner(&cwd, &opts) {
            Ok(pid) => {
                self.log(
                    LogKind::Ok,
                    format!(
                        "restarted {} in {} (pid {pid})",
                        runner.display_id(),
                        display_path(&cwd)
                    ),
                );
                self.refresh();
            }
            Err(err) => self.log(LogKind::Error, format!("restart spawn failed: {err:#}")),
        }
    }

    fn add_dir(&mut self, path: &Path) {
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        match self.client.dirs_add(runner.runner_id_flag(), path) {
            Ok(out) => {
                self.config.remember_dir(path);
                let _ = self.config.save_to(&self.config_path);
                self.log(
                    LogKind::Ok,
                    format!(
                        "added {} to {} {}",
                        display_path(path),
                        runner.display_id(),
                        first_line_or_empty(&out)
                    )
                    .trim()
                    .to_string(),
                );
                self.refresh_dirs_for(&runner);
            }
            Err(err) => self.log(LogKind::Error, format!("dirs add failed: {err:#}")),
        }
    }

    fn remove_selected_dir(&mut self) {
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        let Some(path) = self.selected_dir_path() else {
            self.log(LogKind::Error, "no directory selected");
            return;
        };
        match self.client.dirs_remove(runner.runner_id_flag(), &path) {
            Ok(out) => {
                self.log(
                    LogKind::Ok,
                    format!(
                        "removed {} from {} {}",
                        display_path(&path),
                        runner.display_id(),
                        first_line_or_empty(&out)
                    )
                    .trim()
                    .to_string(),
                );
                self.refresh_dirs_for(&runner);
            }
            Err(err) => self.log(LogKind::Error, format!("dirs remove failed: {err:#}")),
        }
    }

    fn refresh_dirs_for(&mut self, runner: &Runner) {
        match self.client.dirs_list(runner.runner_id_flag()) {
            Ok(dirs) if !dirs.is_empty() => {
                if let Some(r) = self
                    .runners
                    .iter_mut()
                    .find(|r| r.id.eq_ignore_ascii_case(&runner.id))
                {
                    r.dirs = dirs;
                }
            }
            _ => self.refresh(),
        }
        if self.selected_dir >= self.current_dirs().len() {
            self.selected_dir = self.current_dirs().len().saturating_sub(1);
        }
    }

    fn run_update(&mut self) {
        self.pending_update = false;
        match self.client.update() {
            Ok(out) => {
                let summary = summarize_output(&out);
                self.log(LogKind::Ok, format!("amp update: {summary}"));
                self.status = "amp update finished".into();
                if let Ok(v) = self.client.version() {
                    self.amp_version = v;
                }
            }
            Err(err) => {
                self.log(LogKind::Error, format!("amp update failed: {err:#}"));
                self.status = "amp update failed".into();
            }
        }
    }

    fn refresh(&mut self) {
        let current_id = self.current_runner().map(|r| r.id.clone());
        let current_dir = self.selected_dir_path();
        match self.client.list_runners() {
            Ok(list) => {
                self.runners = list;
                self.reselect(current_id.as_deref(), current_dir.as_deref());
                self.status = format!("{} runner(s)", self.runners.len());
            }
            Err(err) => {
                self.log(LogKind::Error, format!("refresh failed: {err:#}"));
            }
        }
        self.last_refresh = Instant::now();
    }

    fn reselect(&mut self, id: Option<&str>, dir: Option<&Path>) {
        if let Some(id) = id {
            if let Some(idx) = self
                .runners
                .iter()
                .position(|r| r.id.eq_ignore_ascii_case(id))
            {
                self.selected_runner = idx;
            }
        }
        if self.selected_runner >= self.runners.len() {
            self.selected_runner = self.runners.len().saturating_sub(1);
        }
        if let Some(dir) = dir {
            if let Some(idx) = self.current_dirs().iter().position(|d| d == dir) {
                self.selected_dir = idx;
            }
        }
        if self.selected_dir >= self.current_dirs().len() {
            self.selected_dir = self.current_dirs().len().saturating_sub(1);
        }
    }

    fn log(&mut self, kind: LogKind, text: impl Into<String>) {
        let text = text.into();
        let time = Local::now().format("%H:%M:%S").to_string();
        self.status = text.clone();
        self.log.push_back(LogLine { time, text, kind });
        while self.log.len() > LOG_CAP {
            self.log.pop_front();
        }
    }
}

fn resolve_stop_pid(runner: &Runner) -> Option<u32> {
    runner.pid.filter(|pid| *pid > 0)
}

fn rebuild_picker(picker: &mut DirPicker, config: &Config) {
    let query = picker.input.trim();
    let mut entries = Vec::new();
    let typed = expand_path(query);
    if !query.is_empty() {
        entries.push(PickerEntry {
            label: format!("use {}", display_path(&typed)),
            path: typed,
            kind: PickerKind::Typed,
        });
    }
    if let Ok(cwd) = std::env::current_dir() {
        entries.push(PickerEntry {
            label: format!("current  {}", display_path(&cwd)),
            path: cwd,
            kind: PickerKind::Special,
        });
    }
    if let Some(home) = dirs::home_dir() {
        entries.push(PickerEntry {
            label: format!("home     {}", display_path(&home)),
            path: home,
            kind: PickerKind::Special,
        });
    }
    for recent in &config.recent_dirs {
        let path = expand_path(recent);
        entries.push(PickerEntry {
            label: format!("recent   {}", display_path(&path)),
            path,
            kind: PickerKind::Recent,
        });
    }
    if let Some(parent) = picker.browse_root.parent() {
        entries.push(PickerEntry {
            label: format!("..       {}", display_path(parent)),
            path: parent.to_path_buf(),
            kind: PickerKind::Browse,
        });
    }
    for child in list_subdirs(&picker.browse_root) {
        let name = child
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| child.display().to_string());
        entries.push(PickerEntry {
            label: format!("browse   {name}/"),
            path: child,
            kind: PickerKind::Browse,
        });
    }
    if !query.is_empty() {
        entries.retain(|e| {
            e.kind == PickerKind::Typed
                || fuzzy_match(query, &e.label)
                || fuzzy_match(query, &e.path.to_string_lossy())
        });
    }
    // Dedup paths, keep first occurrence (typed/special/recent beat browse).
    let mut seen = Vec::new();
    entries.retain(|e| {
        if seen.iter().any(|p| p == &e.path) && e.kind != PickerKind::Typed {
            false
        } else {
            seen.push(e.path.clone());
            true
        }
    });
    picker.entries = entries;
    if picker.cursor >= picker.entries.len() {
        picker.cursor = picker.entries.len().saturating_sub(1);
    }
}

fn list_subdirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(rd) = fs::read_dir(root) else {
        return dirs;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name() {
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
        }
        dirs.push(path);
    }
    dirs.sort();
    dirs
}

pub fn fuzzy_match(query: &str, candidate: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_ascii_lowercase();
    let c = candidate.to_ascii_lowercase();
    if c.contains(&q) {
        return true;
    }
    let mut it = c.chars();
    for qc in q.chars() {
        loop {
            match it.next() {
                Some(cc) if cc == qc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

fn expand_path(s: &str) -> PathBuf {
    let s = s.trim();
    if s.is_empty() {
        return PathBuf::from(".");
    }
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            if stripped.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

fn nonempty_owned(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn flag_text(flags: &StartDefaults, field: FlagField) -> Option<String> {
    match field {
        FlagField::RunnerId => flags.runner_id.clone(),
        FlagField::SettingsFile => flags.settings_file.clone(),
        FlagField::McpConfig => flags.mcp_config.clone(),
        _ => None,
    }
}

fn flag_value_display(flags: &StartDefaults, field: FlagField) -> String {
    match field {
        FlagField::RunnerId => flags.runner_id.clone().unwrap_or_else(|| "(none)".into()),
        FlagField::Mode => flags.mode.clone().unwrap_or_else(|| "(amp default)".into()),
        FlagField::LogLevel => flags
            .log_level
            .clone()
            .unwrap_or_else(|| "(amp default)".into()),
        FlagField::Visibility => flags
            .visibility
            .clone()
            .unwrap_or_else(|| "(amp default)".into()),
        FlagField::SettingsFile => flags
            .settings_file
            .clone()
            .unwrap_or_else(|| "(none)".into()),
        FlagField::McpConfig => flags.mcp_config.clone().unwrap_or_else(|| "(none)".into()),
        FlagField::RemoteControl => on_off(flags.remote_control_terminal),
        FlagField::DiscoverDirs => on_off(flags.discover_dirs),
        FlagField::AmpEnv => on_off(flags.amp_env),
    }
}

fn amp_version_label(v: &str) -> &str {
    v.trim().strip_prefix("amp ").unwrap_or(v.trim())
}

fn on_off(v: bool) -> String {
    if v {
        "on".into()
    } else {
        "off".into()
    }
}

fn first_line_or_empty(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn summarize_output(s: &str) -> String {
    let line = first_line_or_empty(s);
    if line.is_empty() {
        "ok".into()
    } else {
        line
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(1),
        ])
        .split(area);

    draw_title(frame, chunks[0], app);
    draw_main(frame, chunks[1], app);
    draw_log(frame, chunks[2], app);
    draw_status(frame, chunks[3], app);

    match app.overlay {
        Overlay::Help => draw_help(frame, area),
        Overlay::Flags => draw_flags(frame, area, app),
        Overlay::Picker => draw_picker(frame, area, app),
        Overlay::EditField => {
            draw_flags(frame, area, app);
            draw_edit(frame, area, app);
        }
        Overlay::None => {}
    }
}

fn draw_title(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(
            " lazyamp ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{}  ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("amp "),
        Span::styled(
            amp_version_label(&app.amp_version),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            "   q quit  ? help  s start  x stop  r restart  f flags  u update",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), area);
}

fn draw_main(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);
    draw_runners(frame, cols[0], app);
    draw_dirs(frame, cols[1], app);
}

fn pane_block<'a>(title: &'a str, focused: bool) -> Block<'a> {
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(title, style))
}

fn draw_runners(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Runners && app.overlay == Overlay::None;
    let block = pane_block(" runners ", focused);
    if app.runners.is_empty() {
        let empty = Paragraph::new("No runners on this machine.\nPress s to start `amp --no-tui`.")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        frame.render_widget(empty, area);
        return;
    }
    let items: Vec<ListItem> = app
        .runners
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let marker = if i == app.selected_runner { ">" } else { " " };
            let pid = r
                .pid
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "pid ?".into());
            let n = r.dirs.len();
            let src = if r.from_amp_list { "amp" } else { "local" };
            let line = format!(
                "{marker} {:<20}  {pid:<12}  {n} dir{}  [{src}]",
                truncate(r.display_id(), 20),
                if n == 1 { "" } else { "s" }
            );
            let mut style = Style::default();
            if i == app.selected_runner {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            ListItem::new(line).style(style)
        })
        .collect();
    frame.render_widget(List::new(items).block(block), area);
}

fn draw_dirs(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Dirs && app.overlay == Overlay::None;
    let title = match app.current_runner() {
        Some(r) => format!(" dirs · {} ", r.display_id()),
        None => " dirs ".into(),
    };
    let block = pane_block(&title, focused);
    let dirs = app.current_dirs();
    if dirs.is_empty() {
        let empty =
            Paragraph::new("No directories recorded.\nPress a to add (`amp runner dirs add`).")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
        frame.render_widget(empty, area);
        return;
    }
    let items: Vec<ListItem> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let marker = if i == app.selected_dir { ">" } else { " " };
            let mut style = Style::default();
            if i == app.selected_dir {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            ListItem::new(format!("{marker} {}", display_path(d))).style(style)
        })
        .collect();
    frame.render_widget(List::new(items).block(block), area);
}

fn draw_log(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = app
        .log
        .iter()
        .rev()
        .take(inner_h)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|l| {
            let color = match l.kind {
                LogKind::Info => Color::Gray,
                LogKind::Ok => Color::Green,
                LogKind::Error => Color::Red,
            };
            Line::from(vec![
                Span::styled(
                    format!("{}  ", l.time),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(l.text.clone(), Style::default().fg(color)),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(" log ", Style::default().fg(Color::Gray)));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let style = if app.status.to_ascii_lowercase().contains("fail")
        || app.status.to_ascii_lowercase().contains("not found")
        || app.status.to_ascii_lowercase().contains("could not")
    {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(format!(" {}", app.status)).style(style),
        area,
    );
}

fn draw_help(frame: &mut ratatui::Frame, area: Rect) {
    let popup = centered(area, 72, 80);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" help  (esc) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(HELP_TEXT)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_flags(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let popup = centered(area, 70, 70);
    frame.render_widget(Clear, popup);
    let mut lines = vec![
        Line::from("Start defaults (Enter/space to edit or cycle, s save, esc cancel)"),
        Line::from(""),
    ];
    for field in FlagField::ALL {
        let selected = field == app.flag_field;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{marker} {:<28} {}",
                field.label(),
                flag_value_display(&app.flags, field)
            ),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("saved at {}", app.config_path.display()),
        Style::default().fg(Color::DarkGray),
    )));
    let block = Block::default()
        .title(" flags ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn draw_edit(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let popup = centered(area, 60, 20);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(
            " edit {}  (enter confirm, esc cancel) ",
            app.flag_field.label()
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    frame.render_widget(
        Paragraph::new(format!("{}_", app.edit_buffer)).block(block),
        popup,
    );
}

fn draw_picker(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(picker) = app.picker.as_ref() else {
        return;
    };
    let popup = centered(area, 76, 80);
    frame.render_widget(Clear, popup);
    let title = match picker.purpose {
        PickerPurpose::Start => " start runner · pick working directory ",
        PickerPurpose::AddDir => " add directory to selected runner ",
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(popup);
    let outer = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer, popup);

    let input = Paragraph::new(format!("{}_", picker.input)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" filter / path "),
    );
    let inner_input = shrink(chunks[0], 1, 1);
    frame.render_widget(input, inner_input);

    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let marker = if i == picker.cursor { ">" } else { " " };
            let mut style = Style::default();
            if i == picker.cursor {
                style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
            }
            ListItem::new(format!("{marker} {}", e.label)).style(style)
        })
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" matches "));
    frame.render_widget(list, shrink(chunks[1], 1, 0));

    let hint = Paragraph::new("enter select · ←/h parent · →/l browse · type to filter")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, shrink(chunks[2], 1, 0));
}

fn centered(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(v[1])[1]
}

fn shrink(area: Rect, x: u16, y: u16) -> Rect {
    let x = x.min(area.width / 2);
    let y = y.min(area.height / 2);
    Rect {
        x: area.x + x,
        y: area.y + y,
        width: area.width.saturating_sub(x * 2),
        height: area.height.saturating_sub(y * 2),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_contains_and_subsequence() {
        assert!(fuzzy_match("amp", "/home/me/amp-cli"));
        assert!(fuzzy_match("hml", "/home/me/lazy"));
        assert!(!fuzzy_match("zzz", "/home/me"));
        assert!(fuzzy_match("", "anything"));
    }

    #[test]
    fn expand_home_and_plain() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_path("~"), home);
            assert_eq!(expand_path("~/code"), home.join("code"));
        }
        assert_eq!(expand_path("/tmp/x"), PathBuf::from("/tmp/x"));
    }

    #[test]
    fn amp_version_label_strips_prefix() {
        assert_eq!(amp_version_label("amp 0.0.0-stub"), "0.0.0-stub");
        assert_eq!(amp_version_label("1.2.3"), "1.2.3");
    }
}
