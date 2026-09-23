//! Keyboard-first runner manager (ratatui + crossterm).

use crate::amp::{
    is_plausible_runner_id, launch_spec_for_restart, restart_start_options, runner_log_path,
    stop_verified, tail_log_lines, AmpClient, LaunchSpec, Runner, StartHealth, StartOptions,
    StopOutcome,
};
use crate::config::{
    config_path, cycle_choice, Config, StartDefaults, LOG_LEVELS, MODES, VISIBILITIES,
};
use crate::dirpick::{
    collect_ranked_paths, query_zoxide_dirs, walk_matching_dirs, PathSource, RankedPath,
    RankedSearch,
};
use anyhow::Result;
use chrono::Local;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;
use std::collections::VecDeque;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

const REFRESH_EVERY: Duration = Duration::from_secs(3);
const LOG_CAP: usize = 200;
const PICKER_DEBOUNCE: Duration = Duration::from_millis(120);
const HELP_TEXT: &str = "\
lazyamp — manage Amp --no-tui runners

Navigation
  hjkl / arrows     move; h/l also switch panes
  Tab               next pane (runners ↔ served dirs)
  Enter             confirm (picker / flags / typed path)
  Esc               close overlay
  q / Ctrl-c        quit
  ?                 this help

Runners
  s                 start amp --no-tui (pick the process start cwd)
  x                 stop the selected runner (confirms)
  r                 restart selected runner (confirms)
  g                 refresh `amp runner list`

Served directories (paths the runner can see; not the start cwd)
  a                 add a served directory (`amp runner dirs add`)
  d                 remove the selected served directory

Other
  f                 common flags / defaults (saved to config)
  u                 run `amp update`
  c                 show config path

Directory picker: hjkl browse immediate children. / or typing filters
recursively under the browse root, plus recent dirs and `zoxide query -l`
when zoxide is installed.

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
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmAction {
    Stop,
    Restart,
    RemoveDir,
}

enum AmpJob {
    Refresh,
    Update,
    Start {
        cwd: PathBuf,
        opts: StartOptions,
    },
    Stop {
        pid: u32,
        runner_id: Option<String>,
        display_id: String,
    },
    Restart {
        pid: u32,
        runner_id: Option<String>,
        display_id: String,
        spec: LaunchSpec,
    },
    DirsAdd {
        runner_id: Option<String>,
        display_id: String,
        path: PathBuf,
    },
    DirsRemove {
        runner_id: Option<String>,
        display_id: String,
        path: PathBuf,
    },
    DirsList {
        runner_id: Option<String>,
        match_id: String,
    },
}

enum AmpEvent {
    RefreshOk(Vec<Runner>),
    UpdateOk {
        out: String,
        version: Option<String>,
    },
    Started {
        id: String,
        outcome: crate::amp::StartOutcome,
    },
    Stopped {
        id: String,
        outcome: StopOutcome,
        pid: u32,
    },
    Restarted {
        id: String,
        outcome: crate::amp::StartOutcome,
    },
    DirsAddOk {
        runner_id: Option<String>,
        id: String,
        path: PathBuf,
        out: String,
    },
    DirsRemoveOk {
        runner_id: Option<String>,
        id: String,
        path: PathBuf,
        out: String,
    },
    DirsListOk {
        match_id: String,
        dirs: Vec<PathBuf>,
    },
    Failed(String),
}

enum PickerJob {
    Search {
        gen: u64,
        query: String,
        browse_root: PathBuf,
        recent_dirs: Vec<String>,
    },
}

enum PickerEvent {
    Results { gen: u64, entries: Vec<PickerEntry> },
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
}

#[derive(Debug, Clone)]
struct DirPicker {
    purpose: PickerPurpose,
    input: String,
    filter_mode: bool,
    browse_root: PathBuf,
    cursor: usize,
    entries: Vec<PickerEntry>,
    search_gen: u64,
    searching: bool,
    debounce_until: Option<Instant>,
}

struct App {
    config: Config,
    config_path: PathBuf,
    config_writable: bool,
    runners: Vec<Runner>,
    selected_runner: usize,
    selected_dir: usize,
    pane: Pane,
    overlay: Overlay,
    confirm: Option<ConfirmAction>,
    picker: Option<DirPicker>,
    flags: StartDefaults,
    flag_field: FlagField,
    edit_buffer: String,
    log: VecDeque<LogLine>,
    status: String,
    status_kind: LogKind,
    amp_version: String,
    last_refresh: Instant,
    should_quit: bool,
    busy: bool,
    help_scroll: u16,
    job_tx: Sender<AmpJob>,
    ev_rx: Receiver<AmpEvent>,
    picker_tx: Sender<PickerJob>,
    picker_rx: Receiver<PickerEvent>,
}

struct LogLine {
    time: String,
    text: String,
    kind: LogKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Info,
    Ok,
    Error,
}

/// Run the TUI. Restores the terminal on exit or panic.
pub fn run() -> Result<()> {
    let client = AmpClient::detect()?;
    let config_path = config_path();
    let (config, config_writable, load_error) = match Config::load() {
        Ok(loaded) => {
            let writable = loaded.may_write();
            (loaded.config, writable, None)
        }
        Err(err) => (
            Config::default(),
            false,
            Some(format!(
                "invalid config {} — using defaults in memory, will not overwrite: {err:#}",
                config_path.display()
            )),
        ),
    };

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

    let mut app = App::new(client, config, config_path, config_writable);
    if let Some(err) = load_error {
        app.log(LogKind::Error, err);
    }
    let result = app_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn spawn_picker_worker() -> (Sender<PickerJob>, Receiver<PickerEvent>) {
    let (job_tx, job_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lazyamp-dirs".into())
        .spawn(move || picker_worker_loop(job_rx, ev_tx))
        .expect("failed to start directory search thread");
    (job_tx, ev_rx)
}

fn picker_worker_loop(jobs: Receiver<PickerJob>, events: Sender<PickerEvent>) {
    while let Ok(job) = jobs.recv() {
        let PickerJob::Search {
            gen,
            query,
            browse_root,
            recent_dirs,
        } = job;
        let entries = filter_search_entries(&query, &browse_root, &recent_dirs);
        if events.send(PickerEvent::Results { gen, entries }).is_err() {
            break;
        }
    }
}

fn filter_search_entries(
    query: &str,
    browse_root: &Path,
    recent_dirs: &[String],
) -> Vec<PickerEntry> {
    let zoxide = query_zoxide_dirs();
    let nested = walk_matching_dirs(browse_root, query);
    picker_entries_from_sources(query, browse_root, recent_dirs, &zoxide, &nested, false)
}

fn spawn_amp_worker(client: AmpClient) -> (Sender<AmpJob>, Receiver<AmpEvent>) {
    let (job_tx, job_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lazyamp-amp".into())
        .spawn(move || worker_loop(client, job_rx, ev_tx))
        .expect("failed to start Amp worker thread");
    (job_tx, ev_rx)
}

fn worker_loop(client: AmpClient, jobs: Receiver<AmpJob>, events: Sender<AmpEvent>) {
    while let Ok(job) = jobs.recv() {
        let event = run_job(&client, job);
        if events.send(event).is_err() {
            break;
        }
    }
}

fn run_job(client: &AmpClient, job: AmpJob) -> AmpEvent {
    match job {
        AmpJob::Refresh => match client.list_runners() {
            Ok(runners) => AmpEvent::RefreshOk(runners),
            Err(err) => AmpEvent::Failed(format!("refresh failed: {err:#}")),
        },
        AmpJob::Update => match client.update() {
            Ok(out) => {
                let version = client.version().ok();
                AmpEvent::UpdateOk { out, version }
            }
            Err(err) => AmpEvent::Failed(format!("amp update failed: {err:#}")),
        },
        AmpJob::Start { cwd, opts } => {
            let id = opts
                .runner_id
                .clone()
                .unwrap_or_else(|| "(amp-assigned id)".into());
            match client.start_runner(&cwd, &opts) {
                Ok(outcome) => AmpEvent::Started { id, outcome },
                Err(err) => AmpEvent::Failed(format!("start failed: {err:#}")),
            }
        }
        AmpJob::Stop {
            pid,
            runner_id,
            display_id,
        } => match stop_verified(pid, runner_id.as_deref()) {
            Ok(outcome) => AmpEvent::Stopped {
                id: display_id,
                outcome,
                pid,
            },
            Err(err) => AmpEvent::Failed(format!("stop failed: {err:#}")),
        },
        AmpJob::Restart {
            pid,
            runner_id,
            display_id,
            spec,
        } => {
            if let Err(err) = crate::amp::validate_launch_spec(&spec) {
                return AmpEvent::Failed(format!("restart aborted (spec invalid): {err:#}"));
            }
            match stop_verified(pid, runner_id.as_deref()) {
                Ok(_) => {
                    let opts = restart_start_options(&spec);
                    match client.start_runner(&spec.cwd, &opts) {
                        Ok(outcome) => AmpEvent::Restarted {
                            id: display_id,
                            outcome,
                        },
                        Err(err) => AmpEvent::Failed(format!("restart spawn failed: {err:#}")),
                    }
                }
                Err(err) => AmpEvent::Failed(format!("stop before restart failed: {err:#}")),
            }
        }
        AmpJob::DirsAdd {
            runner_id,
            display_id,
            path,
        } => match client.dirs_add(runner_id.as_deref(), &path) {
            Ok(out) => AmpEvent::DirsAddOk {
                runner_id,
                id: display_id,
                path,
                out,
            },
            Err(err) => AmpEvent::Failed(format!("dirs add failed: {err:#}")),
        },
        AmpJob::DirsRemove {
            runner_id,
            display_id,
            path,
        } => match client.dirs_remove(runner_id.as_deref(), &path) {
            Ok(out) => AmpEvent::DirsRemoveOk {
                runner_id,
                id: display_id,
                path,
                out,
            },
            Err(err) => AmpEvent::Failed(format!("dirs remove failed: {err:#}")),
        },
        AmpJob::DirsList {
            runner_id,
            match_id,
        } => match client.dirs_list(runner_id.as_deref()) {
            Ok(dirs) => AmpEvent::DirsListOk { match_id, dirs },
            Err(err) => AmpEvent::Failed(format!("dirs list failed: {err:#}")),
        },
    }
}

fn app_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        app.drain_events();
        app.drain_picker_events();
        app.maybe_dispatch_picker_search();
        terminal.draw(|f| draw(f, app))?;
        if app.should_quit {
            break;
        }
        if event::poll(app.poll_timeout())? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key);
                }
            }
        }
        if app.overlay == Overlay::None && !app.busy && app.last_refresh.elapsed() >= REFRESH_EVERY
        {
            app.request_refresh();
        }
    }
    if app.config_writable {
        let _ = app.config.save_to(&app.config_path);
    }
    Ok(())
}

impl App {
    fn new(client: AmpClient, config: Config, config_path: PathBuf, config_writable: bool) -> Self {
        let flags = config.defaults.clone();
        let amp_version = client.version().unwrap_or_else(|_| "unknown".into());
        let binary = client.binary.display().to_string();
        let (job_tx, ev_rx) = spawn_amp_worker(client);
        let (picker_tx, picker_rx) = spawn_picker_worker();
        let mut app = Self {
            config,
            config_path,
            config_writable,
            runners: Vec::new(),
            selected_runner: 0,
            selected_dir: 0,
            pane: Pane::Runners,
            overlay: Overlay::None,
            confirm: None,
            picker: None,
            flags,
            flag_field: FlagField::RunnerId,
            edit_buffer: String::new(),
            log: VecDeque::new(),
            status: "ready".into(),
            status_kind: LogKind::Info,
            amp_version,
            last_refresh: Instant::now() - REFRESH_EVERY,
            should_quit: false,
            busy: false,
            help_scroll: 0,
            job_tx,
            ev_rx,
            picker_tx,
            picker_rx,
        };
        app.log(
            LogKind::Info,
            format!("Amp {} — {binary}", amp_version_label(&app.amp_version)),
        );
        if !app.config_writable {
            app.log(
                LogKind::Error,
                format!(
                    "config {} is invalid; changes will not be saved",
                    app.config_path.display()
                ),
            );
        }
        app.request_refresh();
        app
    }

    fn submit(&mut self, job: AmpJob, working: impl Into<String>) {
        if self.busy {
            self.log(LogKind::Info, "already working…");
            return;
        }
        let working = working.into();
        self.busy = true;
        self.status = working.clone();
        self.status_kind = LogKind::Info;
        if self.job_tx.send(job).is_err() {
            self.busy = false;
            self.log(LogKind::Error, "Amp worker thread is gone");
        }
    }

    fn request_refresh(&mut self) {
        if self.busy {
            return;
        }
        self.submit(AmpJob::Refresh, "working… refresh");
    }

    fn maybe_save_config(&mut self) {
        if !self.config_writable {
            return;
        }
        if let Err(err) = self.config.save_to(&self.config_path) {
            self.log(LogKind::Error, format!("save failed: {err:#}"));
        }
    }

    fn drain_events(&mut self) {
        loop {
            match self.ev_rx.try_recv() {
                Ok(event) => self.handle_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.busy = false;
                    self.log(LogKind::Error, "Amp worker thread disconnected");
                    break;
                }
            }
        }
    }

    fn poll_timeout(&self) -> Duration {
        if let Some(picker) = &self.picker {
            if let Some(until) = picker.debounce_until {
                let remaining = until.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Duration::from_millis(10);
                }
                return remaining.min(Duration::from_millis(200));
            }
        }
        Duration::from_millis(200)
    }

    fn drain_picker_events(&mut self) {
        loop {
            match self.picker_rx.try_recv() {
                Ok(PickerEvent::Results { gen, entries }) => {
                    let Some(picker) = self.picker.as_mut() else {
                        continue;
                    };
                    if picker.search_gen != gen {
                        continue;
                    }
                    let prev = picker.entries.get(picker.cursor).map(|e| e.path.clone());
                    picker.entries = entries;
                    picker.searching = false;
                    if let Some(prev) = prev {
                        if let Some(idx) = picker.entries.iter().position(|e| e.path == prev) {
                            picker.cursor = idx;
                        } else {
                            picker.cursor = 0;
                        }
                    } else if picker.cursor >= picker.entries.len() {
                        picker.cursor = picker.entries.len().saturating_sub(1);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn maybe_dispatch_picker_search(&mut self) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let Some(until) = picker.debounce_until else {
            return;
        };
        if Instant::now() < until {
            return;
        }
        picker.debounce_until = None;
        let query = picker.input.trim().to_string();
        if query.is_empty() {
            picker.searching = false;
            return;
        }
        let job = PickerJob::Search {
            gen: picker.search_gen,
            query,
            browse_root: picker.browse_root.clone(),
            recent_dirs: expanded_recent_dirs(&self.config),
        };
        if self.picker_tx.send(job).is_err() {
            picker.searching = false;
        }
    }

    fn handle_event(&mut self, event: AmpEvent) {
        self.busy = false;
        match event {
            AmpEvent::RefreshOk(list) => {
                let current_id = self.current_runner().map(|r| r.id.clone());
                let current_dir = self.selected_dir_path();
                self.runners = list;
                self.reselect(current_id.as_deref(), current_dir.as_deref());
                if self.status_kind != LogKind::Error {
                    self.status = format!("{} runner(s)", self.runners.len());
                    self.status_kind = LogKind::Info;
                }
                self.last_refresh = Instant::now();
            }
            AmpEvent::UpdateOk { out, version } => {
                let summary = summarize_output(&out);
                self.log(LogKind::Ok, format!("amp update: {summary}"));
                if let Some(v) = version {
                    self.amp_version = v;
                }
            }
            AmpEvent::Started { id, outcome } => {
                self.config.remember_dir(&outcome.cwd);
                self.maybe_save_config();
                self.log_start_outcome("started", &id, &outcome);
                self.request_refresh();
            }
            AmpEvent::Stopped { id, outcome, pid } => {
                match outcome {
                    StopOutcome::Stopped => {
                        self.log(LogKind::Ok, format!("stopped {id} (pid {pid})"));
                    }
                    StopOutcome::AlreadyGone => {
                        self.log(
                            LogKind::Info,
                            format!("pid {pid} for {id} was already gone"),
                        );
                    }
                }
                self.request_refresh();
            }
            AmpEvent::Restarted { id, outcome } => {
                self.log_start_outcome("restarted", &id, &outcome);
                self.request_refresh();
            }
            AmpEvent::DirsAddOk {
                runner_id,
                id,
                path,
                out,
            } => {
                self.config.remember_dir(&path);
                self.maybe_save_config();
                self.log(
                    LogKind::Ok,
                    format!(
                        "added {} to {id} {}",
                        display_path(&path),
                        first_line_or_empty(&out)
                    )
                    .trim()
                    .to_string(),
                );
                self.submit(
                    AmpJob::DirsList {
                        runner_id,
                        match_id: id,
                    },
                    "working… dirs",
                );
            }
            AmpEvent::DirsRemoveOk {
                runner_id,
                id,
                path,
                out,
            } => {
                self.log(
                    LogKind::Ok,
                    format!(
                        "removed {} from {id} {}",
                        display_path(&path),
                        first_line_or_empty(&out)
                    )
                    .trim()
                    .to_string(),
                );
                self.submit(
                    AmpJob::DirsList {
                        runner_id,
                        match_id: id,
                    },
                    "working… dirs",
                );
            }
            AmpEvent::DirsListOk { match_id, dirs } => {
                if !dirs.is_empty() {
                    if let Some(r) = self
                        .runners
                        .iter_mut()
                        .find(|r| r.id.eq_ignore_ascii_case(&match_id))
                    {
                        r.dirs = dirs;
                    }
                } else {
                    self.request_refresh();
                }
                if self.selected_dir >= self.current_dirs().len() {
                    self.selected_dir = self.current_dirs().len().saturating_sub(1);
                }
            }
            AmpEvent::Failed(err) => self.log(LogKind::Error, err),
        }
    }

    fn log_start_outcome(&mut self, verb: &str, id: &str, outcome: &crate::amp::StartOutcome) {
        let log = display_path(&outcome.log_path);
        match &outcome.health {
            StartHealth::Listed => self.log(
                LogKind::Ok,
                format!("{verb} {id} (pid {}, listed)  log {log}", outcome.pid),
            ),
            StartHealth::Alive => self.log(
                LogKind::Info,
                format!(
                    "spawned {id} (pid {}) — not yet in `amp runner list`; log {log}",
                    outcome.pid
                ),
            ),
            StartHealth::NeedsLogin => self.log(
                LogKind::Error,
                format!(
                    "Amp at pid {} is waiting for login. Run `amp login` in a terminal. log {log}",
                    outcome.pid
                ),
            ),
            StartHealth::Exited { hint } => {
                let hint = hint.trim();
                let extra = if hint.is_empty() {
                    String::new()
                } else {
                    format!(" — {hint}")
                };
                self.log(
                    LogKind::Error,
                    format!(
                        "{id} exited immediately (pid {}). See {log}{extra}",
                        outcome.pid
                    ),
                );
            }
        }
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
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                _ => {}
            },
            Overlay::Flags => self.handle_flags_key(key),
            Overlay::Picker => self.handle_picker_key(key),
            Overlay::EditField => self.handle_edit_key(key),
            Overlay::Confirm => self.handle_confirm_key(key),
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
            KeyCode::Char('g') => self.request_refresh(),
            KeyCode::Char('s') => self.open_picker(PickerPurpose::Start),
            KeyCode::Char('x') => self.ask_confirm(ConfirmAction::Stop),
            KeyCode::Char('r') => self.ask_confirm(ConfirmAction::Restart),
            KeyCode::Char('a') => self.open_picker(PickerPurpose::AddDir),
            KeyCode::Char('d') => self.ask_confirm(ConfirmAction::RemoveDir),
            KeyCode::Char('u') => {
                self.submit(AmpJob::Update, "working… amp update");
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
                if picker.filter_mode || !picker.input.is_empty() {
                    picker.input.clear();
                    picker.filter_mode = false;
                    picker.cursor = 0;
                    refresh_picker(picker, &self.config);
                } else {
                    self.picker = None;
                    self.overlay = Overlay::None;
                }
            }
            KeyCode::Enter => {
                let path = picker_selected_path(picker).or_else(|| {
                    if picker.input.trim().is_empty() {
                        None
                    } else {
                        Some(expand_path(picker.input.trim()))
                    }
                });
                if let Some(path) = path {
                    let purpose = picker.purpose;
                    self.picker = None;
                    self.overlay = Overlay::None;
                    self.finish_picker(purpose, path);
                }
            }
            KeyCode::Right => {
                if let Some(entry) = picker.entries.get(picker.cursor).cloned() {
                    if entry.path.is_dir() {
                        picker.browse_root = entry.path;
                        picker.cursor = 0;
                        refresh_picker(picker, &self.config);
                    }
                }
            }
            KeyCode::Left => {
                if let Some(parent) = picker.browse_root.parent() {
                    picker.browse_root = parent.to_path_buf();
                    picker.cursor = 0;
                    refresh_picker(picker, &self.config);
                }
            }
            KeyCode::Char('l') if key.modifiers.is_empty() && !picker.filter_mode => {
                if let Some(entry) = picker.entries.get(picker.cursor).cloned() {
                    if entry.path.is_dir() {
                        picker.browse_root = entry.path;
                        picker.cursor = 0;
                        refresh_picker(picker, &self.config);
                    }
                }
            }
            KeyCode::Char('h') if key.modifiers.is_empty() && !picker.filter_mode => {
                if let Some(parent) = picker.browse_root.parent() {
                    picker.browse_root = parent.to_path_buf();
                    picker.cursor = 0;
                    refresh_picker(picker, &self.config);
                }
            }
            KeyCode::Down => {
                if picker.cursor + 1 < picker.entries.len() {
                    picker.cursor += 1;
                }
            }
            KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Char('j') if key.modifiers.is_empty() && !picker.filter_mode => {
                if picker.cursor + 1 < picker.entries.len() {
                    picker.cursor += 1;
                }
            }
            KeyCode::Char('k') if key.modifiers.is_empty() && !picker.filter_mode => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Char('/') if key.modifiers.is_empty() && !picker.filter_mode => {
                picker.filter_mode = true;
            }
            KeyCode::Backspace => {
                picker.input.pop();
                if picker.input.is_empty() {
                    picker.filter_mode = false;
                }
                picker.cursor = 0;
                refresh_picker(picker, &self.config);
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && picker_char_is_filter(picker.filter_mode, c) =>
            {
                picker.filter_mode = true;
                picker.input.push(c);
                picker.cursor = 0;
                refresh_picker(picker, &self.config);
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
        if !self.config_writable {
            self.log(
                LogKind::Error,
                format!(
                    "refusing to write invalid config {}",
                    self.config_path.display()
                ),
            );
            return;
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
            filter_mode: false,
            browse_root,
            cursor: 0,
            entries: Vec::new(),
            search_gen: 0,
            searching: false,
            debounce_until: None,
        };
        refresh_picker(&mut picker, &self.config);
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
        self.submit(
            AmpJob::Start {
                cwd: cwd.to_path_buf(),
                opts,
            },
            format!("working… start in {}", display_path(cwd)),
        );
    }

    fn ask_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Stop | ConfirmAction::Restart => {
                if self.current_runner().is_none() {
                    self.log(LogKind::Error, "no runner selected");
                    return;
                }
            }
            ConfirmAction::RemoveDir => {
                if self.current_runner().is_none() {
                    self.log(LogKind::Error, "no runner selected");
                    return;
                }
                if self.selected_dir_path().is_none() {
                    self.log(LogKind::Error, "no directory selected");
                    return;
                }
            }
        }
        self.confirm = Some(action);
        self.overlay = Overlay::Confirm;
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                self.confirm = None;
                self.overlay = Overlay::None;
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let action = self.confirm.take();
                self.overlay = Overlay::None;
                if let Some(action) = action {
                    self.perform_confirm(action);
                }
            }
            _ => {}
        }
    }

    fn perform_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Stop => self.stop_selected(),
            ConfirmAction::Restart => self.restart_selected(),
            ConfirmAction::RemoveDir => self.remove_selected_dir(),
        }
    }

    fn stop_selected(&mut self) {
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        match resolve_stop_pid(&runner) {
            Some(pid) => self.submit(
                AmpJob::Stop {
                    pid,
                    runner_id: runner.runner_id_flag().map(str::to_string),
                    display_id: runner.display_id().to_string(),
                },
                format!("working… stop {}", runner.display_id()),
            ),
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
        let spec = match launch_spec_for_restart(&runner) {
            Ok(spec) => spec,
            Err(err) => {
                self.log(LogKind::Error, format!("cannot restart: {err:#}"));
                return;
            }
        };
        let Some(pid) = resolve_stop_pid(&runner) else {
            self.log(
                LogKind::Error,
                "cannot restart: no PID (see README for PID detection)",
            );
            return;
        };
        self.submit(
            AmpJob::Restart {
                pid,
                runner_id: runner.runner_id_flag().map(str::to_string),
                display_id: runner.display_id().to_string(),
                spec,
            },
            format!("working… restart {}", runner.display_id()),
        );
    }

    fn add_dir(&mut self, path: &Path) {
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        self.submit(
            AmpJob::DirsAdd {
                runner_id: runner.runner_id_flag().map(str::to_string),
                display_id: runner.display_id().to_string(),
                path: path.to_path_buf(),
            },
            format!("working… add {}", display_path(path)),
        );
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
        self.submit(
            AmpJob::DirsRemove {
                runner_id: runner.runner_id_flag().map(str::to_string),
                display_id: runner.display_id().to_string(),
                path,
            },
            format!("working… remove dir from {}", runner.display_id()),
        );
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
        self.status_kind = kind;
        self.log.push_back(LogLine { time, text, kind });
        while self.log.len() > LOG_CAP {
            self.log.pop_front();
        }
    }
}

fn resolve_stop_pid(runner: &Runner) -> Option<u32> {
    runner.pid.filter(|pid| *pid > 0)
}

fn picker_char_is_filter(filter_mode: bool, c: char) -> bool {
    filter_mode || !matches!(c, 'h' | 'j' | 'k' | 'l')
}

fn picker_selected_path(picker: &DirPicker) -> Option<PathBuf> {
    picker.entries.get(picker.cursor).map(|e| e.path.clone())
}

fn refresh_picker(picker: &mut DirPicker, config: &Config) {
    picker.search_gen = picker.search_gen.wrapping_add(1);
    let query = picker.input.trim();
    if query.is_empty() {
        picker.searching = false;
        picker.debounce_until = None;
        picker.filter_mode = picker.filter_mode && !picker.input.is_empty();
        picker.entries = picker_entries_from_sources(
            "",
            &picker.browse_root,
            &expanded_recent_dirs(config),
            &[],
            &[],
            true,
        );
    } else {
        picker.searching = true;
        picker.debounce_until = Some(Instant::now() + PICKER_DEBOUNCE);
        picker.entries = picker_entries_from_sources(
            query,
            &picker.browse_root,
            &expanded_recent_dirs(config),
            &[],
            &[],
            false,
        );
    }
    if picker.cursor >= picker.entries.len() {
        picker.cursor = picker.entries.len().saturating_sub(1);
    }
}

fn expanded_recent_dirs(config: &Config) -> Vec<String> {
    config
        .recent_dirs
        .iter()
        .map(|d| expand_path(d).display().to_string())
        .collect()
}

fn picker_entries_from_sources(
    query: &str,
    browse_root: &Path,
    recent_dirs: &[String],
    zoxide_dirs: &[PathBuf],
    nested_dirs: &[PathBuf],
    browse_children: bool,
) -> Vec<PickerEntry> {
    let cwd = std::env::current_dir().ok();
    let home = dirs::home_dir();
    let typed = {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(expand_path(trimmed))
        }
    };
    let ranked = collect_ranked_paths(RankedSearch {
        query,
        browse_root,
        recent_dirs,
        zoxide_dirs,
        nested_dirs,
        cwd: cwd.as_deref(),
        home: home.as_deref(),
        typed_exact: typed.as_deref(),
        browse_children,
    });
    ranked
        .into_iter()
        .map(|ranked| ranked_to_entry(ranked, browse_root))
        .collect()
}

fn ranked_to_entry(ranked: RankedPath, browse_root: &Path) -> PickerEntry {
    let shown = display_path(&ranked.path);
    let label = match ranked.source {
        PathSource::Current => format!("current  {shown}"),
        PathSource::Home => format!("home     {shown}"),
        PathSource::Recent => format!("recent   {shown}"),
        PathSource::Zoxide => format!("zoxide   {shown}"),
        PathSource::Parent => format!("..       {shown}"),
        PathSource::Browse => {
            let name = ranked
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| shown.clone());
            format!("browse   {name}/")
        }
        PathSource::Nested => {
            if let Ok(rel) = ranked.path.strip_prefix(browse_root) {
                format!("nested   {}/", rel.display())
            } else {
                format!("nested   {shown}")
            }
        }
        PathSource::Exact => format!("exact    {shown}"),
    };
    PickerEntry {
        label,
        path: ranked.path,
    }
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
        Overlay::Help => draw_help(frame, area, app),
        Overlay::Flags => draw_flags(frame, area, app),
        Overlay::Picker => draw_picker(frame, area, app),
        Overlay::EditField => {
            draw_flags(frame, area, app);
            draw_edit(frame, area, app);
        }
        Overlay::Confirm => draw_confirm(frame, area, app),
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
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(cols[1]);
    draw_dirs(frame, right[0], app);
    draw_runner_log(frame, right[1], app);
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
        .map(|r| {
            let pid = r
                .pid
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "pid ?".into());
            let n = r.dirs.len();
            let health = runner_health_label(r);
            let line = format!(
                "{:<20}  {pid:<12}  {n} dir{}  {health}",
                truncate(r.display_id(), 20),
                if n == 1 { "" } else { "s" }
            );
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.selected_runner));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        area,
        &mut state,
    );
}

fn draw_dirs(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let focused = app.pane == Pane::Dirs && app.overlay == Overlay::None;
    let title = match app.current_runner() {
        Some(r) => format!(" served dirs · {} ", r.display_id()),
        None => " served dirs ".into(),
    };
    let block = pane_block(&title, focused);
    let dirs = app.current_dirs();
    if dirs.is_empty() {
        let empty = Paragraph::new(
            "No served directories recorded.\n\
             a adds a served path (`amp runner dirs add`).\n\
             s starts a runner in a working directory (start cwd).",
        )
        .style(Style::default().fg(Color::DarkGray))
        .block(block);
        frame.render_widget(empty, area);
        return;
    }
    let items: Vec<ListItem> = dirs
        .iter()
        .map(|d| ListItem::new(display_path(d)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.selected_dir));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        area,
        &mut state,
    );
}

fn runner_health_label(runner: &Runner) -> &'static str {
    if runner.from_amp_list {
        "connected"
    } else {
        "local only"
    }
}

fn draw_runner_log(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let (title, body) = match app.current_runner() {
        Some(runner) => {
            let path = runner_log_path(runner.runner_id_flag(), runner.cwd.as_deref());
            let lines = tail_log_lines(&path, 8);
            let title = format!(" runner log · {} ", display_path(&path));
            let body = if lines.is_empty() {
                format!("(empty or missing)\n{}", path.display())
            } else {
                lines.join("\n")
            };
            (title, body)
        }
        None => (
            " runner log ".into(),
            "Select a runner to see its last log lines.".into(),
        ),
    };
    let block = pane_block(&title, false);
    frame.render_widget(
        Paragraph::new(body)
            .style(Style::default().fg(Color::Gray))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
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
    let style = match app.status_kind {
        LogKind::Error => Style::default().fg(Color::Red),
        LogKind::Ok => Style::default().fg(Color::Green),
        LogKind::Info => {
            if app.busy {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            }
        }
    };
    frame.render_widget(
        Paragraph::new(format!(" {}", app.status)).style(style),
        area,
    );
}

fn draw_help(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let popup = centered(area, 72, 80);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" help  (esc · j/k scroll) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(
        Paragraph::new(HELP_TEXT)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.help_scroll, 0)),
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
        PickerPurpose::Start => " start runner · pick start cwd ",
        PickerPurpose::AddDir => " add served directory to selected runner ",
    };
    let outer = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer, popup);
    let inner = popup.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);

    let mode = if picker.searching {
        "searching"
    } else if picker.filter_mode {
        "filter"
    } else {
        "nav"
    };
    let input = Paragraph::new(format!("{}_", picker.input)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {mode} / path ")),
    );
    frame.render_widget(input, chunks[0]);

    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .map(|e| ListItem::new(e.label.clone()))
        .collect();
    let mut state = ListState::default();
    if !picker.entries.is_empty() {
        state.select(Some(picker.cursor.min(picker.entries.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" matches "))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> "),
        chunks[1],
        &mut state,
    );

    let hint = Paragraph::new(
        "/ recursive filter · zoxide if installed · hjkl browse children (nav) · enter select",
    )
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(hint, chunks[2]);
}

fn draw_confirm(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let Some(action) = app.confirm else {
        return;
    };
    let popup = centered(area, 60, 24);
    frame.render_widget(Clear, popup);
    let question = match action {
        ConfirmAction::Stop => {
            let id = app
                .current_runner()
                .map(|r| r.display_id().to_string())
                .unwrap_or_else(|| "?".into());
            format!("Stop runner `{id}`?  SIGTERM then SIGKILL if needed.")
        }
        ConfirmAction::Restart => {
            let id = app
                .current_runner()
                .map(|r| r.display_id().to_string())
                .unwrap_or_else(|| "?".into());
            format!("Restart runner `{id}` using its saved launch spec?")
        }
        ConfirmAction::RemoveDir => {
            let path = app
                .selected_dir_path()
                .map(|p| display_path(&p))
                .unwrap_or_else(|| "?".into());
            format!("Remove served directory `{path}` from the selected runner?")
        }
    };
    let block = Block::default()
        .title(" confirm  (y/enter · n/esc) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(question),
            Line::from(""),
            Line::from(Span::styled(
                "y / Enter  confirm     n / Esc  cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(block)
        .wrap(Wrap { trim: true }),
        popup,
    );
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
        assert!(crate::dirpick::fuzzy_match("amp", "/home/me/amp-cli"));
        assert!(crate::dirpick::fuzzy_match("hml", "/home/me/lazy"));
        assert!(!crate::dirpick::fuzzy_match("zzz", "/home/me"));
        assert!(crate::dirpick::fuzzy_match("", "anything"));
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

    #[test]
    fn picker_hjkl_type_in_filter_mode_only() {
        assert!(!picker_char_is_filter(false, 'h'));
        assert!(!picker_char_is_filter(false, 'j'));
        assert!(picker_char_is_filter(false, 's'));
        assert!(picker_char_is_filter(true, 'h'));
        assert!(picker_char_is_filter(true, 'l'));
    }
}
