//! Runner manager TUI (ratatui + crossterm): keyboard first, mouse works too.

mod view;

use crate::amp::{
    is_plausible_runner_id, launch_spec_for_restart, local_amp_pids, merge_with_local,
    removal_restart_spec, restart_start_options, saved_launch_spec, stop_verified, tail_log_lines,
    AmpClient, LaunchSpec, Runner, StartHealth, StartOptions, StopOutcome,
};
use crate::config::{
    config_path, cycle_choice, Config, StartDefaults, LOG_LEVELS, MODES, VISIBILITIES,
};
use crate::dirpick::{
    collect_ranked_paths, query_zoxide_dirs, walk_matching_dirs, PathSource, RankedPath,
    RankedSearch,
};
use crate::update;
use anyhow::Result;
use chrono::Local;
use crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

const LOG_CAP: usize = 200;
const PICKER_DEBOUNCE: Duration = Duration::from_millis(120);
/// Local `/proc` scan + log reads: cheap, catches starts and stops quickly.
const LOCAL_POLL: Duration = Duration::from_secs(2);
/// `amp runner list` spawns Amp (~150 ms, ~200 MB peak RSS): run it rarely.
const AMP_LIST_EVERY: Duration = Duration::from_secs(15);
const UPDATE_CHECK_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const TOAST_FOR: Duration = Duration::from_secs(5);
const ERROR_TOAST_FOR: Duration = Duration::from_secs(12);
const ACTIVITY_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Runners,
    Dirs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Log,
    Flags,
    Picker,
    EditField,
    Confirm,
}

/// A confirmed action carries its target, so a refresh that changes the
/// selection while the dialog is open cannot redirect it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmAction {
    Stop(Runner),
    Restart(Runner),
    RemoveDir {
        runner: Runner,
        path: PathBuf,
        restart: Option<Box<LaunchSpec>>,
    },
    SelfUpdate,
    /// lazyamp was replaced on disk; quit so the user starts the new version.
    /// (No in-place `exec`: it would drop the threads that reap runners this
    /// process started, leaving zombies that break a later stop/restart.)
    QuitAfterUpdate(String),
}

enum AmpJob {
    Update,
    SelfUpdate {
        exe: PathBuf,
    },
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
}

enum AmpEvent {
    UpdateOk {
        out: String,
        version: Option<String>,
    },
    SelfUpToDate {
        latest: String,
    },
    SelfUpdated(update::InstallOutcome),
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
        id: String,
        path: PathBuf,
        out: String,
    },
    DirsRemoveOk {
        id: String,
        path: PathBuf,
        out: String,
    },
    Failed(String),
}

/// Requests to the background runner poller.
enum PollCmd {
    /// Refresh now, including `amp runner list`.
    Now,
    /// Terminal focus changed; polling pauses while unfocused.
    Focus(bool),
}

/// Results from the background runner poller. Never touches the status line
/// unless something failed.
enum PollEvent {
    Snapshot {
        runners: Vec<Runner>,
        list_error: Option<String>,
    },
    AmpVersion(String),
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerPurpose {
    Start,
    /// Captured when the picker opens, so a refresh that changes the selection
    /// cannot redirect the directory to another runner.
    AddDir(Runner),
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

    fn name(self) -> &'static str {
        match self {
            FlagField::RunnerId => "Runner ID",
            FlagField::Mode => "Mode",
            FlagField::LogLevel => "Log level",
            FlagField::Visibility => "Thread visibility",
            FlagField::SettingsFile => "Settings file",
            FlagField::McpConfig => "MCP config",
            FlagField::RemoteControl => "Remote control terminal",
            FlagField::DiscoverDirs => "Discover directories",
            FlagField::AmpEnv => "Use Amp env",
        }
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
    /// Where the match came from (`recent`, `zoxide`, …).
    tag: &'static str,
    text: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct DirPicker {
    purpose: PickerPurpose,
    input: TextInput,
    filter_mode: bool,
    browse_root: PathBuf,
    cursor: usize,
    entries: Vec<PickerEntry>,
    search_gen: u64,
    searching: bool,
    debounce_until: Option<Instant>,
}

/// Single-line text field with a real cursor (byte index on a char boundary).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TextInput {
    text: String,
    cursor: usize,
}

impl TextInput {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    /// Apply an editing key. Returns true when the text changed.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.replace_range(..self.cursor, "");
                self.cursor = 0;
                return true;
            }
            KeyCode::Char(c) if !ctrl => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                return true;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let prev = self.prev_boundary();
                self.text.replace_range(prev..self.cursor, "");
                self.cursor = prev;
                return true;
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                let next = self.next_boundary();
                self.text.replace_range(self.cursor..next, "");
                return true;
            }
            KeyCode::Left => self.cursor = self.prev_boundary(),
            KeyCode::Right => self.cursor = self.next_boundary(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => {}
        }
        false
    }
}

/// Clickable areas recorded by the last draw, used to route mouse events.
#[derive(Debug, Default)]
struct HitMap {
    runners: ListHit,
    dirs: ListHit,
    picker: ListHit,
    flags: ListHit,
    /// The open dialog; clicks outside it close it.
    popup: Option<Rect>,
    /// Footer hints and dialog buttons, each mapped to the key it stands for.
    buttons: Vec<(Rect, KeyCode)>,
}

/// A rendered list: its inner area (one row per item) and scroll offset.
#[derive(Debug, Default, Clone, Copy)]
struct ListHit {
    area: Rect,
    offset: usize,
    len: usize,
}

impl ListHit {
    fn index_at(&self, col: u16, row: u16) -> Option<usize> {
        if !self.area.contains(Position::new(col, row)) {
            return None;
        }
        let idx = self.offset + usize::from(row - self.area.y);
        (idx < self.len).then_some(idx)
    }
}

/// Cached tail of the selected runner's log; re-read only when the file changes.
#[derive(Debug, Default)]
struct LogTail {
    path: Option<PathBuf>,
    stamp: Option<(u64, SystemTime)>,
    lines: Vec<String>,
}

struct Toast {
    text: String,
    kind: LogKind,
    until: Instant,
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
    edit: TextInput,
    log: VecDeque<LogLine>,
    toast: Option<Toast>,
    amp_version: String,
    should_quit: bool,
    /// Label of the user action running on the Amp worker, if any.
    busy: Option<String>,
    /// A manual refresh (`g`) is in flight.
    refreshing: bool,
    /// Last `amp runner list` failure, shown once until it changes or clears.
    list_error: Option<String>,
    help_scroll: u16,
    log_scroll: usize,
    runner_log: LogTail,
    /// Newer lazyamp release tag, when one exists.
    latest_release: Option<String>,
    /// This executable, resolved at startup (before an update can replace it).
    exe: Option<PathBuf>,
    /// Printed after the terminal is restored (e.g. "run lazyamp again").
    exit_note: Option<String>,
    started: Instant,
    hits: RefCell<HitMap>,
    job_tx: Sender<AmpJob>,
    ev_rx: Receiver<AmpEvent>,
    poll_tx: Sender<PollCmd>,
    poll_rx: Receiver<PollEvent>,
    update_rx: Receiver<String>,
    picker_tx: Sender<PickerJob>,
    picker_rx: Receiver<PickerEvent>,
}

struct LogLine {
    time: String,
    text: String,
    kind: LogKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));

    let mut app = App::new(client, config, config_path, config_writable);
    if let Some(err) = load_error {
        app.log(LogKind::Error, err);
    }
    let result = app_loop(&mut terminal, &mut app);

    restore_terminal();
    terminal.show_cursor()?;
    result?;
    if let Some(note) = &app.exit_note {
        println!("{note}");
    }
    Ok(())
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableFocusChange,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
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
        AmpJob::Update => match client.update() {
            Ok(out) => {
                let version = client.version().ok();
                AmpEvent::UpdateOk { out, version }
            }
            Err(err) => AmpEvent::Failed(format!("amp update failed: {err:#}")),
        },
        AmpJob::SelfUpdate { exe } => {
            let latest = match update::check_latest_cached(Duration::ZERO) {
                Ok(tag) => tag,
                Err(err) => return AmpEvent::Failed(format!("update check failed: {err:#}")),
            };
            if !update::is_newer(update::current_version(), &latest) {
                return AmpEvent::SelfUpToDate { latest };
            }
            match update::install(&exe, Some(&latest)) {
                Ok(outcome) => AmpEvent::SelfUpdated(outcome),
                Err(err) => AmpEvent::Failed(format!("lazyamp update failed: {err:#}")),
            }
        }
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
                id: display_id,
                path,
                out,
            },
            Err(err) => AmpEvent::Failed(format!("dirs remove failed: {err:#}")),
        },
    }
}

fn spawn_poller(client: AmpClient) -> (Sender<PollCmd>, Receiver<PollEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lazyamp-poll".into())
        .spawn(move || poller_loop(client, cmd_rx, ev_tx))
        .expect("failed to start runner poller thread");
    (cmd_tx, ev_rx)
}

/// Two-tier polling: every [`LOCAL_POLL`] scan local processes and logs (no
/// subprocess); run `amp runner list` only every [`AMP_LIST_EVERY`], when the
/// set of local runner PIDs changes, or on request. Only changed snapshots are sent.
fn poller_loop(client: AmpClient, cmds: Receiver<PollCmd>, events: Sender<PollEvent>) {
    if let Ok(version) = client.version() {
        if events.send(PollEvent::AmpVersion(version)).is_err() {
            return;
        }
    }
    let mut listed: Vec<Runner> = Vec::new();
    let mut list_error: Option<String> = None;
    let mut last_list: Option<Instant> = None;
    let mut last_pids: Option<Vec<u32>> = None;
    let mut last_sent: Option<(Vec<Runner>, Option<String>)> = None;
    let mut focused = true;
    let mut force = true;
    loop {
        if focused || force {
            let pids = local_amp_pids();
            let stale = last_list.is_none_or(|t| t.elapsed() >= AMP_LIST_EVERY);
            if force || stale || last_pids.as_ref() != Some(&pids) {
                match client.list_from_amp() {
                    Ok(runners) => {
                        listed = runners;
                        list_error = None;
                    }
                    Err(err) => {
                        listed.clear();
                        list_error = Some(format!("{err:#}"));
                    }
                }
                last_list = Some(Instant::now());
            }
            last_pids = Some(pids);
            let snapshot = (merge_with_local(listed.clone()), list_error.clone());
            // A forced refresh always answers, so a manual refresh can finish.
            if force || last_sent.as_ref() != Some(&snapshot) {
                let (runners, list_error) = snapshot.clone();
                if events
                    .send(PollEvent::Snapshot {
                        runners,
                        list_error,
                    })
                    .is_err()
                {
                    return;
                }
                last_sent = Some(snapshot);
            }
            force = false;
        }
        match cmds.recv_timeout(LOCAL_POLL) {
            Ok(cmd) => {
                for cmd in std::iter::once(cmd).chain(cmds.try_iter()) {
                    match cmd {
                        PollCmd::Now => force = true,
                        // Regaining focus wakes the loop; the usual staleness
                        // and PID-change rules decide whether to call Amp.
                        PollCmd::Focus(now_focused) => focused = now_focused,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Resolve the latest lazyamp release off the UI thread (cached for a day).
fn spawn_update_check() -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if std::env::var_os("LAZYAMP_NO_UPDATE_CHECK").is_none() {
        let _ = std::thread::Builder::new()
            .name("lazyamp-update-check".into())
            .spawn(move || {
                if let Ok(tag) = update::check_latest_cached(UPDATE_CHECK_MAX_AGE) {
                    let _ = tx.send(tag);
                }
            });
    }
    rx
}

fn app_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        app.drain_events();
        app.drain_picker_events();
        app.maybe_dispatch_picker_search();
        app.refresh_runner_log();
        terminal.draw(|f| view::draw(f, app))?;
        if app.should_quit {
            break;
        }
        if event::poll(app.poll_timeout())? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::FocusGained => app.poll(PollCmd::Focus(true)),
                Event::FocusLost => app.poll(PollCmd::Focus(false)),
                _ => {}
            }
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
        let binary = client.binary.clone();
        let (poll_tx, poll_rx) = spawn_poller(client.clone());
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
            edit: TextInput::default(),
            log: VecDeque::new(),
            toast: None,
            amp_version: String::new(),
            should_quit: false,
            busy: None,
            refreshing: true,
            list_error: None,
            help_scroll: 0,
            log_scroll: 0,
            runner_log: LogTail::default(),
            latest_release: None,
            exe: update::current_exe().ok(),
            exit_note: None,
            started: Instant::now(),
            hits: RefCell::new(HitMap::default()),
            job_tx,
            ev_rx,
            poll_tx,
            poll_rx,
            update_rx: spawn_update_check(),
            picker_tx,
            picker_rx,
        };
        app.history(LogKind::Info, format!("amp binary {}", binary.display()));
        if !app.config_writable {
            app.log(
                LogKind::Error,
                format!(
                    "config {} is invalid; changes will not be saved",
                    app.config_path.display()
                ),
            );
        }
        app
    }

    fn submit(&mut self, job: AmpJob, working: impl Into<String>) {
        if let Some(current) = &self.busy {
            let text = format!("still busy: {current}");
            self.log(LogKind::Info, text);
            return;
        }
        self.busy = Some(working.into());
        if self.job_tx.send(job).is_err() {
            self.busy = None;
            self.log(LogKind::Error, "Amp worker thread is gone");
        }
    }

    fn poll(&mut self, cmd: PollCmd) {
        let _ = self.poll_tx.send(cmd);
    }

    /// Manual refresh (`g`): the header shows a spinner until the snapshot arrives.
    fn request_refresh(&mut self) {
        self.refreshing = true;
        self.poll(PollCmd::Now);
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
                    if self.busy.take().is_some() {
                        self.log(LogKind::Error, "Amp worker thread disconnected");
                    }
                    break;
                }
            }
        }
        while let Ok(event) = self.poll_rx.try_recv() {
            self.handle_poll(event);
        }
        while let Ok(tag) = self.update_rx.try_recv() {
            if update::is_newer(update::current_version(), &tag) {
                self.latest_release = Some(tag);
            }
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|t| Instant::now() >= t.until)
        {
            self.toast = None;
        }
    }

    fn handle_poll(&mut self, event: PollEvent) {
        match event {
            PollEvent::AmpVersion(version) => self.amp_version = version,
            PollEvent::Snapshot {
                runners,
                list_error,
            } => {
                self.refreshing = false;
                let current_id = self.current_runner().map(|r| r.id.clone());
                let current_dir = self.selected_dir_path();
                self.runners = runners;
                self.reselect(current_id.as_deref(), current_dir.as_deref());
                if list_error != self.list_error {
                    if let Some(err) = &list_error {
                        let text = if self.runners.is_empty() {
                            format!("amp runner list failed: {err}")
                        } else {
                            format!("amp runner list failed; showing local processes: {err}")
                        };
                        self.log(LogKind::Error, text);
                    }
                    self.list_error = list_error;
                }
            }
        }
    }

    /// Re-read the selected runner's log only when its size or mtime changed.
    fn refresh_runner_log(&mut self) {
        let path = self.current_runner().and_then(|r| r.log_path.clone());
        let stamp = path.as_deref().and_then(|p| {
            let meta = std::fs::metadata(p).ok()?;
            Some((meta.len(), meta.modified().ok()?))
        });
        if path == self.runner_log.path && stamp == self.runner_log.stamp {
            return;
        }
        self.runner_log.lines = path
            .as_deref()
            .map(|p| tail_log_lines(p, ACTIVITY_LINES))
            .unwrap_or_default();
        self.runner_log.path = path;
        self.runner_log.stamp = stamp;
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
        if self.busy.is_some() || self.refreshing {
            // Keep the spinner moving.
            return Duration::from_millis(100);
        }
        Duration::from_millis(250)
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
        let query = picker.input.text.trim().to_string();
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
        self.busy = None;
        match event {
            AmpEvent::UpdateOk { out, version } => {
                let summary = summarize_output(&out);
                if let Some(v) = version {
                    self.amp_version = v;
                }
                self.log(
                    LogKind::Ok,
                    format!("amp update: {summary}. Running runners keep the old version until restarted (↻)."),
                );
                self.request_refresh();
            }
            AmpEvent::SelfUpToDate { latest } => {
                self.latest_release = None;
                self.log(
                    LogKind::Ok,
                    format!(
                        "lazyamp {} is up to date (latest {latest})",
                        update::current_version()
                    ),
                );
            }
            AmpEvent::SelfUpdated(outcome) => {
                self.latest_release = None;
                self.history(LogKind::Info, summarize_output(&outcome.output));
                self.log(
                    LogKind::Ok,
                    format!(
                        "installed lazyamp {} at {}",
                        outcome.tag,
                        display_path(&outcome.path)
                    ),
                );
                self.confirm = Some(ConfirmAction::QuitAfterUpdate(outcome.tag));
                self.overlay = Overlay::Confirm;
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
                        self.log(LogKind::Ok, format!("stopped {id}"));
                    }
                    StopOutcome::AlreadyGone => {
                        self.log(
                            LogKind::Info,
                            format!("{id} (pid {pid}) had already exited"),
                        );
                    }
                }
                self.request_refresh();
            }
            AmpEvent::Restarted { id, outcome } => {
                self.log_start_outcome("restarted", &id, &outcome);
                self.request_refresh();
            }
            AmpEvent::DirsAddOk { id, path, out } => {
                self.config.remember_dir(&path);
                self.maybe_save_config();
                self.history(LogKind::Info, summarize_output(&out));
                self.log(
                    LogKind::Ok,
                    format!("{id} now serves {}", display_path(&path)),
                );
                self.request_refresh();
            }
            AmpEvent::DirsRemoveOk { id, path, out } => {
                self.history(LogKind::Info, summarize_output(&out));
                self.log(
                    LogKind::Ok,
                    format!("{id} no longer serves {}", display_path(&path)),
                );
                self.request_refresh();
            }
            AmpEvent::Failed(err) => self.log(LogKind::Error, err),
        }
    }

    fn log_start_outcome(&mut self, verb: &str, id: &str, outcome: &crate::amp::StartOutcome) {
        self.history(
            LogKind::Info,
            format!(
                "{verb} {id}: pid {}, log {}",
                outcome.pid,
                display_path(&outcome.log_path)
            ),
        );
        match &outcome.health {
            StartHealth::Listed => self.log(LogKind::Ok, format!("{verb} {id}")),
            StartHealth::Alive => self.log(
                LogKind::Info,
                format!("{verb} {id}; waiting for it to register with Amp"),
            ),
            StartHealth::NeedsLogin => self.log(
                LogKind::Error,
                format!("{id} is waiting for login: run `amp login` in a terminal"),
            ),
            StartHealth::Exited { hint } => {
                for line in hint.lines().filter(|l| !l.trim().is_empty()) {
                    self.history(LogKind::Error, line.to_string());
                }
                self.log(
                    LogKind::Error,
                    format!("{id} exited right after starting (L shows its log)"),
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
            Overlay::Log => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('L') => {
                    self.overlay = Overlay::None;
                }
                KeyCode::Char('j') | KeyCode::Down => self.scroll_log(-1),
                KeyCode::Char('k') | KeyCode::Up => self.scroll_log(1),
                _ => {}
            },
            Overlay::Flags => self.handle_flags_key(key),
            Overlay::Picker => self.handle_picker_key(key),
            Overlay::EditField => self.handle_edit_key(key),
            Overlay::Confirm => self.handle_confirm_key(key),
            Overlay::None => self.handle_normal_key(key),
        }
    }

    /// Log history scrolls from the bottom: `log_scroll` lines above the newest.
    fn scroll_log(&mut self, delta: i32) {
        let max = self.log.len().saturating_sub(1) as i32;
        self.log_scroll = (self.log_scroll as i32 + delta).clamp(0, max) as usize;
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let (col, row) = (mouse.column, mouse.row);
        let scroll = match mouse.kind {
            MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::Down(MouseButton::Left) => 0,
            _ => return,
        };
        if scroll != 0 {
            self.handle_scroll(col, row, scroll);
            return;
        }
        let hits = self.hits.borrow();
        let button = hits
            .buttons
            .iter()
            .find(|(area, _)| area.contains(Position::new(col, row)))
            .map(|(_, code)| *code);
        let outside_popup = hits
            .popup
            .is_some_and(|p| !p.contains(Position::new(col, row)));
        let (runner_hit, dir_hit) = (
            hits.runners.index_at(col, row),
            hits.dirs.index_at(col, row),
        );
        let (picker_hit, flag_hit) = (
            hits.picker.index_at(col, row),
            hits.flags.index_at(col, row),
        );
        drop(hits);

        if let Some(code) = button {
            self.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            return;
        }
        match self.overlay {
            Overlay::None => {
                if let Some(idx) = runner_hit {
                    if self.selected_runner != idx {
                        self.selected_dir = 0;
                    }
                    self.pane = Pane::Runners;
                    self.selected_runner = idx;
                } else if let Some(idx) = dir_hit {
                    self.pane = Pane::Dirs;
                    self.selected_dir = idx;
                }
            }
            Overlay::Picker => {
                if let (Some(idx), Some(picker)) = (picker_hit, self.picker.as_mut()) {
                    if picker.cursor == idx {
                        // Second click on the highlighted entry picks it.
                        self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    } else {
                        picker.cursor = idx;
                    }
                } else if outside_popup {
                    self.picker = None;
                    self.overlay = Overlay::None;
                }
            }
            Overlay::Flags => {
                if let Some(idx) = flag_hit {
                    let field = FlagField::ALL[idx];
                    if self.flag_field == field {
                        self.activate_flag_field();
                    } else {
                        self.flag_field = field;
                    }
                } else if outside_popup {
                    self.overlay = Overlay::None;
                }
            }
            Overlay::Confirm if outside_popup => {
                self.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            Overlay::Help | Overlay::Log if outside_popup => self.overlay = Overlay::None,
            Overlay::EditField if outside_popup => self.overlay = Overlay::Flags,
            _ => {}
        }
    }

    fn handle_scroll(&mut self, col: u16, row: u16, delta: i32) {
        match self.overlay {
            Overlay::Help => {
                self.help_scroll = self.help_scroll.saturating_add_signed(delta as i16);
            }
            Overlay::Log => self.scroll_log(-delta),
            Overlay::Picker => {
                if let Some(picker) = self.picker.as_mut() {
                    let max = picker.entries.len().saturating_sub(1) as i32;
                    picker.cursor = (picker.cursor as i32 + delta).clamp(0, max) as usize;
                }
            }
            Overlay::Flags => {
                self.flag_field = if delta > 0 {
                    self.flag_field.next()
                } else {
                    self.flag_field.prev()
                };
            }
            Overlay::None => {
                let hits = self.hits.borrow();
                let pane = if hits.dirs.area.contains(Position::new(col, row)) {
                    Pane::Dirs
                } else if hits.runners.area.contains(Position::new(col, row)) {
                    Pane::Runners
                } else {
                    return;
                };
                drop(hits);
                self.pane = pane;
                self.move_sel(delta);
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => {
                self.help_scroll = 0;
                self.overlay = Overlay::Help;
            }
            KeyCode::Char('L') => {
                self.log_scroll = 0;
                self.overlay = Overlay::Log;
            }
            KeyCode::Tab | KeyCode::BackTab => self.cycle_pane(),
            KeyCode::Char('h') | KeyCode::Left => self.pane = Pane::Runners,
            KeyCode::Char('l') | KeyCode::Right => self.pane = Pane::Dirs,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::Char('g') => self.request_refresh(),
            KeyCode::Char('s') => self.open_picker(PickerPurpose::Start),
            KeyCode::Char('x') => {
                if let Some(runner) = self.current_runner().cloned() {
                    self.ask_confirm(ConfirmAction::Stop(runner));
                } else {
                    self.log(LogKind::Error, "no runner selected");
                }
            }
            KeyCode::Char('r') => {
                if let Some(runner) = self.current_runner().cloned() {
                    self.ask_confirm(ConfirmAction::Restart(runner));
                } else {
                    self.log(LogKind::Error, "no runner selected");
                }
            }
            KeyCode::Char('a') => match self.current_runner().cloned() {
                Some(runner) => self.open_picker(PickerPurpose::AddDir(runner)),
                None => self.log(LogKind::Error, "select a runner before adding a directory"),
            },
            KeyCode::Char('d') => self.ask_remove_dir(),
            KeyCode::Char('u') => self.submit(AmpJob::Update, "updating amp"),
            KeyCode::Char('U') => self.ask_confirm(ConfirmAction::SelfUpdate),
            KeyCode::Char('f') => {
                self.flags = self.config.defaults.clone();
                self.flag_field = FlagField::RunnerId;
                self.overlay = Overlay::Flags;
            }
            KeyCode::Char('c') => {
                let text = format!("config: {}", self.config_path.display());
                self.log(LogKind::Info, text);
            }
            KeyCode::Enter if self.pane == Pane::Dirs => {
                if let Some(dir) = self.selected_dir_path() {
                    self.log(LogKind::Info, dir.display().to_string());
                }
            }
            _ => {}
        }
    }

    fn handle_flags_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.overlay = Overlay::None,
            KeyCode::Char('j') | KeyCode::Down => self.flag_field = self.flag_field.next(),
            KeyCode::Char('k') | KeyCode::Up => self.flag_field = self.flag_field.prev(),
            KeyCode::Char('s') => self.save_flags(),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate_flag_field(),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('h') | KeyCode::Left => {
                self.nudge_flag()
            }
            _ => {}
        }
    }

    /// Like fzf: typing always edits the query; ↑↓ move; ←→ browse while the
    /// query is empty; Tab enters the highlighted directory.
    fn handle_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            self.overlay = Overlay::None;
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let empty = picker.input.text.is_empty();
        match key.code {
            KeyCode::Esc => {
                if empty {
                    self.picker = None;
                    self.overlay = Overlay::None;
                } else {
                    picker.input.clear();
                    picker.filter_mode = false;
                    picker.cursor = 0;
                    refresh_picker(picker, &self.config);
                }
            }
            KeyCode::Enter => {
                let typed = picker.input.text.trim();
                let path = picker_selected_path(picker)
                    .or_else(|| (!typed.is_empty()).then(|| expand_path(typed)));
                if let Some(path) = path {
                    let purpose = picker.purpose.clone();
                    self.picker = None;
                    self.overlay = Overlay::None;
                    self.finish_picker(purpose, path);
                }
            }
            KeyCode::Down => picker_move(picker, 1),
            KeyCode::Up => picker_move(picker, -1),
            KeyCode::Char('j' | 'n') if ctrl => picker_move(picker, 1),
            KeyCode::Char('k' | 'p') if ctrl => picker_move(picker, -1),
            KeyCode::Right if empty => picker_browse_into(picker, &self.config),
            KeyCode::Left if empty => picker_browse_up(picker, &self.config),
            KeyCode::Tab => picker_browse_into(picker, &self.config),
            _ => {
                if picker.input.handle_key(key) {
                    picker.filter_mode = !picker.input.text.is_empty();
                    picker.cursor = 0;
                    refresh_picker(picker, &self.config);
                }
            }
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.overlay = Overlay::Flags,
            KeyCode::Enter => {
                self.commit_edit();
                self.overlay = Overlay::Flags;
            }
            _ => {
                self.edit.handle_key(key);
            }
        }
    }

    fn activate_flag_field(&mut self) {
        if self.flag_field.is_text() {
            self.edit = TextInput::new(flag_text(&self.flags, self.flag_field).unwrap_or_default());
            self.overlay = Overlay::EditField;
        } else if self.flag_field.is_cycle() {
            self.nudge_flag();
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

    fn nudge_flag(&mut self) {
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
        let value = nonempty_owned(&self.edit.text);
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
        let browse_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut picker = DirPicker {
            purpose,
            input: TextInput::default(),
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
            PickerPurpose::AddDir(runner) => self.add_dir(&runner, &path),
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
            format!("starting a runner in {}", display_path(cwd)),
        );
    }

    fn ask_confirm(&mut self, action: ConfirmAction) {
        self.confirm = Some(action);
        self.overlay = Overlay::Confirm;
    }

    fn ask_remove_dir(&mut self) {
        if self.busy.is_some() {
            self.log(
                LogKind::Info,
                "wait for the current operation before removing a directory",
            );
            return;
        }
        let Some(runner) = self.current_runner().cloned() else {
            self.log(LogKind::Error, "no runner selected");
            return;
        };
        let Some(path) = self.selected_dir_path() else {
            self.log(LogKind::Error, "no directory selected");
            return;
        };
        let saved = saved_launch_spec(&runner);
        let restart = match removal_restart_spec(&runner, &path, saved) {
            Ok(spec) => spec.map(Box::new),
            Err(err) => {
                self.log(LogKind::Error, format!("cannot remove directory: {err:#}"));
                return;
            }
        };
        if restart.is_some() && resolve_stop_pid(&runner).is_none() {
            self.log(
                LogKind::Error,
                "cannot change primary directory: no verified local PID",
            );
            return;
        }
        self.ask_confirm(ConfirmAction::RemoveDir {
            runner,
            path,
            restart,
        });
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
            ConfirmAction::Stop(runner) => self.stop(&runner),
            ConfirmAction::Restart(runner) => self.restart(&runner),
            ConfirmAction::RemoveDir {
                runner,
                path,
                restart,
            } => {
                self.remove_dir(runner, path, restart.map(|spec| *spec));
            }
            ConfirmAction::SelfUpdate => match self.exe.clone() {
                Some(exe) => self.submit(AmpJob::SelfUpdate { exe }, "updating lazyamp"),
                None => self.log(
                    LogKind::Error,
                    "cannot locate the lazyamp executable; run `lazyamp update` in a shell",
                ),
            },
            ConfirmAction::QuitAfterUpdate(tag) => {
                self.exit_note = Some(format!(
                    "lazyamp {tag} is installed. Run `lazyamp` to start it; your Amp runners kept running."
                ));
                self.should_quit = true;
            }
        }
    }

    fn stop(&mut self, runner: &Runner) {
        match resolve_stop_pid(runner) {
            Some(pid) => self.submit(
                AmpJob::Stop {
                    pid,
                    runner_id: runner.runner_id_flag().map(str::to_string),
                    display_id: runner.display_id().to_string(),
                },
                format!("stopping {}", runner.display_id()),
            ),
            None => self.log(
                LogKind::Error,
                format!(
                    "no local process found for {}; it may run on another machine",
                    runner.display_id()
                ),
            ),
        }
    }

    fn restart(&mut self, runner: &Runner) {
        let spec = match launch_spec_for_restart(runner) {
            Ok(spec) => spec,
            Err(err) => {
                self.log(LogKind::Error, format!("cannot restart: {err:#}"));
                return;
            }
        };
        let Some(pid) = resolve_stop_pid(runner) else {
            self.log(
                LogKind::Error,
                format!(
                    "cannot restart {}: no local process found",
                    runner.display_id()
                ),
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
            format!("restarting {}", runner.display_id()),
        );
    }

    fn add_dir(&mut self, runner: &Runner, path: &Path) {
        self.submit(
            AmpJob::DirsAdd {
                runner_id: runner.runner_id_flag().map(str::to_string),
                display_id: runner.display_id().to_string(),
                path: path.to_path_buf(),
            },
            format!("adding {}", display_path(path)),
        );
    }

    fn remove_dir(&mut self, runner: Runner, path: PathBuf, restart: Option<LaunchSpec>) {
        if let Some(spec) = restart {
            let Some(pid) = resolve_stop_pid(&runner) else {
                self.log(
                    LogKind::Error,
                    "cannot change primary directory: no local PID",
                );
                return;
            };
            let message = format!(
                "removing {} and restarting in {}",
                display_path(&path),
                display_path(&spec.cwd)
            );
            self.submit(
                AmpJob::Restart {
                    pid,
                    runner_id: runner.runner_id_flag().map(str::to_string),
                    display_id: runner.display_id().to_string(),
                    spec,
                },
                message,
            );
            return;
        }
        self.submit(
            AmpJob::DirsRemove {
                runner_id: runner.runner_id_flag().map(str::to_string),
                display_id: runner.display_id().to_string(),
                path,
            },
            format!("removing a directory from {}", runner.display_id()),
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

    /// Record a message in the history (`L`) and show it briefly in the status line.
    fn log(&mut self, kind: LogKind, text: impl Into<String>) {
        let text = text.into();
        let until = Instant::now()
            + if kind == LogKind::Error {
                ERROR_TOAST_FOR
            } else {
                TOAST_FOR
            };
        self.toast = Some(Toast {
            text: text.clone(),
            kind,
            until,
        });
        self.history(kind, text);
    }

    /// Record a message in the history only.
    fn history(&mut self, kind: LogKind, text: impl Into<String>) {
        let time = Local::now().format("%H:%M:%S").to_string();
        self.log.push_back(LogLine {
            time,
            text: text.into(),
            kind,
        });
        while self.log.len() > LOG_CAP {
            self.log.pop_front();
        }
    }
}

fn resolve_stop_pid(runner: &Runner) -> Option<u32> {
    runner.pid.filter(|pid| *pid > 0)
}

fn picker_move(picker: &mut DirPicker, delta: i32) {
    let max = picker.entries.len().saturating_sub(1) as i32;
    picker.cursor = (picker.cursor as i32 + delta).clamp(0, max) as usize;
}

fn picker_browse_into(picker: &mut DirPicker, config: &Config) {
    if let Some(entry) = picker.entries.get(picker.cursor).cloned() {
        if entry.path.is_dir() {
            picker.browse_root = entry.path;
            picker.input.clear();
            picker.filter_mode = false;
            picker.cursor = 0;
            refresh_picker(picker, config);
        }
    }
}

fn picker_browse_up(picker: &mut DirPicker, config: &Config) {
    if let Some(parent) = picker.browse_root.parent() {
        picker.browse_root = parent.to_path_buf();
        picker.cursor = 0;
        refresh_picker(picker, config);
    }
}

fn picker_selected_path(picker: &DirPicker) -> Option<PathBuf> {
    picker.entries.get(picker.cursor).map(|e| e.path.clone())
}

fn refresh_picker(picker: &mut DirPicker, config: &Config) {
    picker.search_gen = picker.search_gen.wrapping_add(1);
    let query = picker.input.text.trim();
    if query.is_empty() {
        picker.searching = false;
        picker.debounce_until = None;
        picker.filter_mode = picker.filter_mode && !picker.input.text.is_empty();
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
    let (tag, text) = match ranked.source {
        PathSource::Current => ("current", shown),
        PathSource::Home => ("home", shown),
        PathSource::Recent => ("recent", shown),
        PathSource::Zoxide => ("zoxide", shown),
        PathSource::Parent => ("up", format!("../  {shown}")),
        PathSource::Browse => {
            let name = ranked
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(shown);
            ("", format!("{name}/"))
        }
        PathSource::Nested => match ranked.path.strip_prefix(browse_root) {
            Ok(rel) => ("", format!("{}/", rel.display())),
            Err(_) => ("", shown),
        },
        PathSource::Exact => ("typed", shown),
    };
    PickerEntry {
        tag,
        text,
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

/// Path with the home directory shown as `~`. Also matches the resolved home
/// (e.g. `/var/home/me` when `/home` is a symlink, as on Fedora Atomic).
fn display_path(path: &Path) -> String {
    static HOMES: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    let homes = HOMES.get_or_init(|| {
        let mut homes: Vec<PathBuf> = dirs::home_dir().into_iter().collect();
        if let Some(real) = homes.first().and_then(|h| std::fs::canonicalize(h).ok()) {
            if !homes.contains(&real) {
                homes.push(real);
            }
        }
        homes
    });
    for home in homes {
        if let Ok(stripped) = path.strip_prefix(home) {
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

/// `amp 0.0.17-g1 (released …)` → `0.0.17-g1`.
fn amp_version_label(v: &str) -> &str {
    let v = v.trim();
    let v = v.strip_prefix("amp ").unwrap_or(v);
    v.split_whitespace().next().unwrap_or(v)
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
        assert_eq!(
            amp_version_label("0.0.1792022479-ge66e24 (released 2026-10-15T00:01:19.000Z, 1h ago)"),
            "0.0.1792022479-ge66e24"
        );
    }

    fn test_app() -> (App, Receiver<AmpJob>, Receiver<PollCmd>) {
        let (job_tx, jobs) = mpsc::channel();
        let (poll_tx, polls) = mpsc::channel();
        let (_, ev_rx) = mpsc::channel();
        let (_, poll_rx) = mpsc::channel();
        let (_, update_rx) = mpsc::channel();
        let (picker_tx, _) = mpsc::channel();
        let (_, picker_rx) = mpsc::channel();
        (
            App {
                config: Config::default(),
                config_path: PathBuf::new(),
                config_writable: false,
                runners: Vec::new(),
                selected_runner: 0,
                selected_dir: 0,
                pane: Pane::Dirs,
                overlay: Overlay::None,
                confirm: None,
                picker: None,
                flags: StartDefaults::default(),
                flag_field: FlagField::RunnerId,
                edit: TextInput::default(),
                log: VecDeque::new(),
                toast: None,
                amp_version: String::new(),
                should_quit: false,
                busy: None,
                refreshing: false,
                list_error: None,
                help_scroll: 0,
                log_scroll: 0,
                runner_log: LogTail::default(),
                latest_release: None,
                exe: None,
                exit_note: None,
                started: Instant::now(),
                hits: RefCell::new(HitMap::default()),
                job_tx,
                ev_rx,
                poll_tx,
                poll_rx,
                update_rx,
                picker_tx,
                picker_rx,
            },
            jobs,
            polls,
        )
    }

    fn runner(id: &str, pid: u32) -> Runner {
        Runner {
            id: id.into(),
            pid: Some(pid),
            cwd: Some(PathBuf::from("/old")),
            dirs: vec![PathBuf::from("/old"), PathBuf::from("/next")],
            from_amp_list: true,
            from_local_scan: true,
            ..Runner::default()
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn directory_confirmation_can_cancel_and_keeps_the_confirmed_target() {
        for restart in [false, true] {
            let (mut app, jobs, _) = test_app();
            let runner = runner("confirmed-box", 4321);
            let spec = LaunchSpec {
                runner_id: Some(runner.id.clone()),
                cwd: PathBuf::from("/next"),
                options: StartOptions::default(),
            };
            let action = ConfirmAction::RemoveDir {
                runner: runner.clone(),
                path: PathBuf::from("/old"),
                restart: restart.then(|| Box::new(spec.clone())),
            };
            app.runners.push(runner);
            app.ask_confirm(action.clone());
            assert!(
                jobs.try_recv().is_err(),
                "opening confirmation must do no work"
            );
            app.handle_confirm_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert!(app.confirm.is_none());
            assert!(jobs.try_recv().is_err(), "cancel must do no work");
            app.ask_confirm(action);
            app.runners.clear(); // A changed selection must not redirect the confirmed operation.
            app.handle_confirm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            match jobs.try_recv().unwrap() {
                AmpJob::Restart {
                    pid,
                    runner_id,
                    spec: actual,
                    ..
                } if restart => {
                    assert_eq!(pid, 4321);
                    assert_eq!(runner_id.as_deref(), Some("confirmed-box"));
                    assert_eq!(actual, spec);
                }
                AmpJob::DirsRemove {
                    runner_id, path, ..
                } if !restart => {
                    assert_eq!(runner_id.as_deref(), Some("confirmed-box"));
                    assert_eq!(path, PathBuf::from("/old"));
                }
                _ => panic!("wrong removal job"),
            }
        }
    }

    #[test]
    fn successful_dir_removal_refreshes_the_authoritative_list() {
        let (mut app, _, polls) = test_app();
        app.handle_event(AmpEvent::DirsRemoveOk {
            id: "box".into(),
            path: PathBuf::from("/old"),
            out: "removed".into(),
        });
        assert!(matches!(polls.try_recv().unwrap(), PollCmd::Now));
    }

    #[test]
    fn stop_confirmation_targets_the_runner_it_was_opened_for() {
        let (mut app, jobs, _) = test_app();
        app.runners = vec![runner("alpha", 1111), runner("beta", 2222)];
        app.pane = Pane::Runners;
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.overlay, Overlay::Confirm);
        // A background refresh drops alpha while the dialog is open; the
        // selection now points at beta.
        app.handle_poll(PollEvent::Snapshot {
            runners: vec![runner("beta", 2222)],
            list_error: None,
        });
        press(&mut app, KeyCode::Char('y'));
        match jobs.try_recv().unwrap() {
            AmpJob::Stop { pid, runner_id, .. } => {
                assert_eq!(pid, 1111);
                assert_eq!(runner_id.as_deref(), Some("alpha"));
            }
            _ => panic!("expected a stop job"),
        }
    }

    #[test]
    fn add_dir_picker_targets_the_runner_it_was_opened_for() {
        let (mut app, jobs, _) = test_app();
        let tmp = tempfile::tempdir().unwrap();
        app.runners = vec![runner("alpha", 1111), runner("beta", 2222)];
        app.pane = Pane::Runners;
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.overlay, Overlay::Picker);
        app.handle_poll(PollEvent::Snapshot {
            runners: vec![runner("beta", 2222)],
            list_error: None,
        });
        for c in tmp.path().display().to_string().chars() {
            press(&mut app, KeyCode::Char(c));
        }
        app.picker.as_mut().unwrap().entries.clear(); // use the typed path
        press(&mut app, KeyCode::Enter);
        match jobs.try_recv().unwrap() {
            AmpJob::DirsAdd {
                runner_id, path, ..
            } => {
                assert_eq!(runner_id.as_deref(), Some("alpha"));
                assert_eq!(path, tmp.path());
            }
            _ => panic!("expected a dirs add job"),
        }
    }

    #[test]
    fn background_refresh_is_silent_and_never_blocks_actions() {
        let (mut app, jobs, _) = test_app();
        app.runners = vec![runner("alpha", 1111)];
        app.pane = Pane::Runners;
        for _ in 0..3 {
            app.handle_poll(PollEvent::Snapshot {
                runners: vec![runner("alpha", 1111)],
                list_error: None,
            });
        }
        assert!(
            app.toast.is_none(),
            "refresh must not touch the status line"
        );
        assert!(app.log.is_empty());
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(jobs.try_recv().unwrap(), AmpJob::Stop { .. }));

        // A failing `amp runner list` is reported once, not on every poll.
        for _ in 0..3 {
            app.handle_poll(PollEvent::Snapshot {
                runners: vec![runner("alpha", 1111)],
                list_error: Some("boom".into()),
            });
        }
        assert_eq!(
            app.log.iter().filter(|l| l.text.contains("boom")).count(),
            1
        );
    }

    #[test]
    fn mouse_clicks_select_rows_and_press_footer_hints() {
        use ratatui::backend::TestBackend;
        let (mut app, _, _) = test_app();
        app.runners = vec![runner("alpha", 1111), runner("beta", 2222)];
        app.pane = Pane::Runners;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| view::draw(f, &app)).unwrap();

        let click = |col, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let rows = app.hits.borrow().runners.area;
        app.handle_mouse(click(rows.x + 3, rows.y + 1));
        assert_eq!(app.current_runner().unwrap().id, "beta");

        let dirs = app.hits.borrow().dirs.area;
        app.handle_mouse(click(dirs.x + 3, dirs.y + 1));
        assert_eq!(app.pane, Pane::Dirs);
        assert_eq!(app.selected_dir, 1);

        terminal.draw(|f| view::draw(f, &app)).unwrap();
        let help = app
            .hits
            .borrow()
            .buttons
            .iter()
            .find(|(_, code)| *code == KeyCode::Char('?'))
            .map(|(rect, _)| *rect)
            .expect("help hint in the footer");
        app.handle_mouse(click(help.x, help.y));
        assert_eq!(app.overlay, Overlay::Help);
    }

    #[test]
    fn text_input_edits_at_the_cursor() {
        let mut input = TextInput::new("~/códe");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Backspace)); // removes the multibyte 'ó'
        assert_eq!(input.text, "~/cde");
        input.handle_key(key(KeyCode::Char('o')));
        assert_eq!(input.text, "~/code");
        assert_eq!(input.cursor, "~/co".len());
        input.handle_key(key(KeyCode::Home));
        input.handle_key(key(KeyCode::Delete));
        assert_eq!(input.text, "/code");
    }

    #[test]
    fn picker_typing_always_searches_even_with_hjkl() {
        let (mut app, _, _) = test_app();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("child")).unwrap();
        app.open_picker(PickerPurpose::Start);
        let picker = app.picker.as_mut().unwrap();
        picker.browse_root = tmp.path().to_path_buf();
        refresh_picker(picker, &app.config);
        for c in "lazy".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.input.text, "lazy");
        assert_eq!(picker.browse_root, tmp.path(), "typing must not browse");
        // Clearing the query brings back arrow browsing.
        press(&mut app, KeyCode::Esc);
        let picker = app.picker.as_mut().unwrap();
        picker.cursor = picker
            .entries
            .iter()
            .position(|e| e.path == tmp.path().join("child"))
            .expect("child listed");
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.picker.as_ref().unwrap().browse_root,
            tmp.path().join("child")
        );
    }

    #[test]
    fn restart_revalidates_remaining_directories_before_stopping() {
        let tmp = tempfile::tempdir().unwrap();
        let event = run_job(
            &AmpClient {
                binary: PathBuf::from("must-not-run"),
            },
            AmpJob::Restart {
                pid: 1, // Even an invalid signal target must not be reached.
                runner_id: Some("box".into()),
                display_id: "box".into(),
                spec: LaunchSpec {
                    runner_id: Some("box".into()),
                    cwd: tmp.path().to_path_buf(),
                    options: StartOptions {
                        extra_dirs: vec![tmp.path().join("disappeared-after-confirmation")],
                        ..StartOptions::default()
                    },
                },
            },
        );
        let AmpEvent::Failed(error) = event else {
            panic!("invalid restart succeeded")
        };
        assert!(error.contains("restart aborted (spec invalid)"), "{error}");
        assert!(error.contains("served directory does not exist"), "{error}");
    }
}
