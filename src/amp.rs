//! Amp CLI wrapper: argument builders, list parsers, and process control.

use crate::config::state_dir;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Options used when spawning `amp --no-tui`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartOptions {
    pub runner_id: Option<String>,
    pub mode: Option<String>,
    pub log_level: Option<String>,
    pub settings_file: Option<String>,
    pub mcp_config: Option<String>,
    pub visibility: Option<String>,
    pub remote_control_terminal: bool,
    pub extra_dirs: Vec<PathBuf>,
    pub discover_dirs: bool,
    pub amp_env: bool,
}

/// A runner discovered from `amp runner list` and/or local process scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub id: String,
    pub pid: Option<u32>,
    pub cwd: Option<PathBuf>,
    pub dirs: Vec<PathBuf>,
    pub from_amp_list: bool,
    pub from_local_scan: bool,
}

impl Runner {
    pub fn display_id(&self) -> &str {
        if self.id.is_empty() {
            "(unnamed)"
        } else {
            &self.id
        }
    }

    pub fn runner_id_flag(&self) -> Option<&str> {
        if self.id.is_empty() {
            None
        } else {
            Some(self.id.as_str())
        }
    }
}

/// Local `amp --no-tui` process found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProcess {
    pub pid: u32,
    pub runner_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub extra_dirs: Vec<PathBuf>,
}

/// Located Amp binary plus helpers to invoke it.
#[derive(Debug, Clone)]
pub struct AmpClient {
    pub binary: PathBuf,
}

impl AmpClient {
    /// Find `amp` on `PATH`, or `AMP_BIN` if set.
    pub fn detect() -> Result<Self> {
        let specified = std::env::var_os("AMP_BIN").map(PathBuf::from);
        let binary = specified.unwrap_or_else(|| PathBuf::from("amp"));
        match resolve_executable(&binary) {
            Some(path) => Ok(Self { binary: path }),
            None => bail!("{}", missing_amp_message(&binary)),
        }
    }

    pub fn version(&self) -> Result<String> {
        let out = self.run_ok(&["version"])?;
        Ok(first_line(&out))
    }

    /// List runners via `amp runner list`, then attach PIDs from a local scan.
    pub fn list_runners(&self) -> Result<Vec<Runner>> {
        let listed = match self.list_from_amp() {
            Ok(v) => v,
            Err(err) => {
                let local = scan_local_amp_processes();
                if local.is_empty() {
                    return Err(err);
                }
                return Ok(merge_runners(Vec::new(), &local, &load_spawn_registry()));
            }
        };
        Ok(merge_runners(
            listed,
            &scan_local_amp_processes(),
            &load_spawn_registry(),
        ))
    }

    fn list_from_amp(&self) -> Result<Vec<Runner>> {
        if let Ok(out) = self.run_ok(&["runner", "list", "--json"]) {
            let parsed = parse_runner_list(&out)?;
            if !parsed.is_empty() || looks_like_json(&out) {
                return Ok(parsed);
            }
        }
        let out = self.run_ok(&["runner", "list"])?;
        parse_runner_list(&out)
    }

    pub fn dirs_list(&self, runner_id: Option<&str>) -> Result<Vec<PathBuf>> {
        let args = dirs_list_args(runner_id);
        let out = self.run_ok_owned(&args)?;
        Ok(parse_dirs_list(&out))
    }

    pub fn dirs_add(&self, runner_id: Option<&str>, path: &Path) -> Result<String> {
        let args = dirs_add_args(runner_id, path);
        self.run_ok_owned(&args)
    }

    pub fn dirs_remove(&self, runner_id: Option<&str>, path: &Path) -> Result<String> {
        let args = dirs_remove_args(runner_id, path);
        self.run_ok_owned(&args)
    }

    pub fn update(&self) -> Result<String> {
        let args = update_args();
        self.run_ok_owned(&args)
    }

    /// Spawn `amp --no-tui` in `cwd` and return the new PID.
    pub fn start_runner(&self, cwd: &Path, opts: &StartOptions) -> Result<u32> {
        if !cwd.is_dir() {
            bail!("working directory does not exist: {}", cwd.display());
        }
        let args = start_runner_args(opts);
        let log_path = runner_log_path(opts.runner_id.as_deref(), cwd);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let log_file = fs::File::create(&log_path)
            .with_context(|| format!("failed to create {}", log_path.display()))?;
        let err_file = log_file
            .try_clone()
            .context("failed to clone runner log file")?;

        let mut cmd = Command::new(&self.binary);
        cmd.args(&args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(err_file));

        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {} {args:?}", self.binary.display()))?;
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        record_spawn(pid, opts.runner_id.as_deref(), cwd);
        Ok(pid)
    }

    fn run_ok(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .with_context(|| {
                format!(
                    "failed to execute {} {}",
                    self.binary.display(),
                    args.join(" ")
                )
            })?;
        finish_output(args, &output)
    }

    fn run_ok_owned(&self, args: &[String]) -> Result<String> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_ok(&refs)
    }
}

fn finish_output(args: &[&str], output: &std::process::Output) -> Result<String> {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "`amp {}` failed ({}): {} {}",
        args.join(" "),
        output.status,
        stdout.trim(),
        stderr.trim()
    );
}

/// Build argv for `amp --no-tui` (without the binary name).
pub fn start_runner_args(opts: &StartOptions) -> Vec<String> {
    let mut args = vec!["--no-tui".to_string()];
    push_opt(&mut args, "--runner-id", opts.runner_id.as_deref());
    push_opt(&mut args, "--mode", opts.mode.as_deref());
    push_opt(&mut args, "--log-level", opts.log_level.as_deref());
    push_opt(&mut args, "--settings-file", opts.settings_file.as_deref());
    push_opt(&mut args, "--mcp-config", opts.mcp_config.as_deref());
    push_opt(&mut args, "--visibility", opts.visibility.as_deref());
    if opts.remote_control_terminal {
        args.push("--remote-control-terminal".into());
    }
    if opts.discover_dirs {
        args.push("--discover-dirs".into());
    }
    if opts.amp_env {
        args.push("--amp-env".into());
    }
    for dir in &opts.extra_dirs {
        args.push("--dir".into());
        args.push(dir.display().to_string());
    }
    args
}

pub fn dirs_list_args(runner_id: Option<&str>) -> Vec<String> {
    let mut args = vec!["runner".into(), "dirs".into(), "list".into()];
    push_opt(&mut args, "--runner-id", runner_id);
    args
}

pub fn dirs_add_args(runner_id: Option<&str>, path: &Path) -> Vec<String> {
    let mut args = vec!["runner".into(), "dirs".into(), "add".into()];
    push_opt(&mut args, "--runner-id", runner_id);
    args.push(path.display().to_string());
    args
}

pub fn dirs_remove_args(runner_id: Option<&str>, path: &Path) -> Vec<String> {
    let mut args = vec!["runner".into(), "dirs".into(), "remove".into()];
    push_opt(&mut args, "--runner-id", runner_id);
    args.push(path.display().to_string());
    args
}

pub fn update_args() -> Vec<String> {
    vec!["update".into()]
}

pub fn list_runners_args(json: bool) -> Vec<String> {
    let mut args = vec!["runner".into(), "list".into()];
    if json {
        args.push("--json".into());
    }
    args
}

fn push_opt(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|s| !s.is_empty()) {
        args.push(flag.into());
        args.push(v.to_string());
    }
}

/// Parse `amp runner list` stdout (JSON or text).
pub fn parse_runner_list(output: &str) -> Result<Vec<Runner>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if looks_like_json(trimmed) {
        return parse_runner_json(trimmed);
    }
    Ok(parse_runner_text(output))
}

fn looks_like_json(s: &str) -> bool {
    let s = s.trim_start();
    s.starts_with('{') || s.starts_with('[')
}

fn parse_runner_json(raw: &str) -> Result<Vec<Runner>> {
    let value: Value = serde_json::from_str(raw).context("amp runner list --json is not JSON")?;
    let items = match &value {
        Value::Array(arr) => arr.clone(),
        Value::Object(map) => map
            .get("runners")
            .or_else(|| map.get("items"))
            .or_else(|| map.get("data"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![value.clone()]),
        _ => Vec::new(),
    };
    Ok(items.into_iter().filter_map(runner_from_json).collect())
}

fn runner_from_json(value: Value) -> Option<Runner> {
    let Value::Object(map) = value else {
        return None;
    };
    let id =
        first_string(&map, &["id", "runnerId", "runner_id", "name", "runner"]).unwrap_or_default();
    let pid = first_pid(&map, &["pid", "PID", "processId", "process_id"]);
    let cwd = first_string(
        &map,
        &["cwd", "workdir", "workingDirectory", "working_directory"],
    )
    .map(PathBuf::from);
    let mut dirs = first_string_list(&map, &["dirs", "directories", "servedDirs", "served_dirs"]);
    if dirs.is_empty() {
        if let Some(dir) = first_string(&map, &["dir", "directory", "path"]) {
            dirs.push(PathBuf::from(dir));
        }
    }
    if let Some(cwd) = &cwd {
        if !dirs.iter().any(|d| d == cwd) {
            dirs.insert(0, cwd.clone());
        }
    }
    if id.is_empty() && pid.is_none() && dirs.is_empty() {
        return None;
    }
    Some(Runner {
        id,
        pid,
        cwd,
        dirs,
        from_amp_list: true,
        from_local_scan: false,
    })
}

fn first_string(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(v) = map.get(*key) {
            match v {
                Value::String(s) if !s.trim().is_empty() => return Some(s.clone()),
                Value::Number(n) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

fn first_pid(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u32> {
    for key in keys {
        if let Some(v) = map.get(*key) {
            if let Some(pid) = json_pid(v) {
                return Some(pid);
            }
        }
    }
    None
}

fn json_pid(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => parse_pid_token(s),
        _ => None,
    }
}

fn first_string_list(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Vec<PathBuf> {
    for key in keys {
        if let Some(v) = map.get(*key) {
            match v {
                Value::Array(arr) => {
                    return arr
                        .iter()
                        .filter_map(|item| item.as_str().map(PathBuf::from))
                        .collect();
                }
                Value::String(s) => return split_dir_list(s),
                _ => {}
            }
        }
    }
    Vec::new()
}

/// Heuristic parser for human-readable `amp runner list` tables.
pub fn parse_runner_text(output: &str) -> Vec<Runner> {
    let mut runners = Vec::new();
    let mut current: Option<Runner> = None;
    let mut header_cols: Option<Vec<String>> = None;

    for raw_line in output.lines() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if is_noise_line(line) {
            continue;
        }
        if looks_like_header(line) {
            header_cols = Some(split_columns(line));
            continue;
        }

        let indented = raw_line.starts_with(' ') || raw_line.starts_with('\t');
        if indented {
            if let Some(runner) = current.as_mut() {
                for dir in extract_paths(line) {
                    push_unique_dir(&mut runner.dirs, dir);
                }
                if runner.pid.is_none() {
                    runner.pid = extract_pid(line);
                }
            }
            continue;
        }

        if let Some(finished) = current.take() {
            runners.push(finished);
        }

        let mut runner = runner_from_text_line(line, header_cols.as_deref());
        if runner.dirs.is_empty() {
            runner.dirs = extract_paths(line);
        }
        if runner.pid.is_none() {
            runner.pid = extract_pid(line);
        }
        if runner.id.is_empty() {
            runner.id = infer_id_from_line(line);
        }
        current = Some(runner);
    }
    if let Some(finished) = current {
        runners.push(finished);
    }
    runners
        .into_iter()
        .filter(|r| !r.id.is_empty() || r.pid.is_some() || !r.dirs.is_empty())
        .collect()
}

fn runner_from_text_line(line: &str, header: Option<&[String]>) -> Runner {
    let mut runner = Runner {
        id: String::new(),
        pid: None,
        cwd: None,
        dirs: Vec::new(),
        from_amp_list: true,
        from_local_scan: false,
    };
    if let Some(headers) = header {
        let cols = split_columns(line);
        for (header, value) in headers.iter().zip(cols.iter()) {
            let h = header.to_ascii_lowercase();
            if matches!(
                h.as_str(),
                "id" | "runner" | "runner-id" | "runner_id" | "name"
            ) {
                runner.id = value.clone();
            } else if h.contains("pid") {
                runner.pid = parse_pid_token(value);
            } else if h.contains("dir") || h.contains("path") || h.contains("cwd") {
                runner.dirs.extend(split_dir_list(value));
            }
        }
    }
    runner
}

fn infer_id_from_line(line: &str) -> String {
    let cleaned = line
        .split("pid")
        .next()
        .unwrap_or(line)
        .split("PID")
        .next()
        .unwrap_or(line);
    for token in cleaned.split_whitespace() {
        if token.starts_with('/') || token.starts_with('~') || token.contains(',') {
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if token.contains('=') {
            continue;
        }
        return token.trim_matches(|c| c == ':' || c == ',').to_string();
    }
    String::new()
}

fn is_noise_line(line: &str) -> bool {
    let t = line.trim();
    t.chars().all(|c| c == '-' || c == '=' || c == ' ')
        || t.eq_ignore_ascii_case("runners")
        || t.to_ascii_lowercase().ends_with(" runner(s)")
        || t.to_ascii_lowercase().ends_with(" runners")
}

fn looks_like_header(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let has_id = lower.split_whitespace().any(|w| {
        matches!(
            w.trim_matches(|c| c == ':' || c == ','),
            "id" | "runner" | "runner-id" | "runner_id" | "name"
        )
    });
    let has_meta = ["pid", "dir", "cwd", "path"]
        .iter()
        .any(|k| lower.split_whitespace().any(|w| w.contains(k)));
    has_id && has_meta
}

fn split_columns(line: &str) -> Vec<String> {
    line.split(['\t', '|'])
        .flat_map(|part| {
            if part.contains("  ") {
                part.split("  ")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            } else {
                vec![part.trim().to_string()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect()
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn extract_paths(line: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for token in line.split(|c: char| c == ',' || c.is_whitespace()) {
        let token = token.trim().trim_matches('"');
        if token.starts_with('/') || token.starts_with("~/") || token == "~" {
            dirs.push(PathBuf::from(token));
        }
    }
    if dirs.is_empty() {
        if let Some(rest) = line.split(':').nth(1) {
            dirs.extend(split_dir_list(rest));
        }
    }
    dirs
}

fn split_dir_list(raw: &str) -> Vec<PathBuf> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn extract_pid(line: &str) -> Option<u32> {
    let lower = line.to_ascii_lowercase();
    for key in ["pid=", "pid:", "pid "] {
        if let Some(idx) = lower.find(key) {
            let rest = &line[idx + key.len()..];
            let token = rest
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())?;
            return parse_pid_token(token);
        }
    }
    None
}

fn parse_pid_token(token: &str) -> Option<u32> {
    token
        .trim()
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
        .filter(|pid| *pid > 0)
}

fn push_unique_dir(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if !dirs.iter().any(|d| d == &dir) {
        dirs.push(dir);
    }
}

/// Parse `amp runner dirs list` stdout.
pub fn parse_dirs_list(output: &str) -> Vec<PathBuf> {
    let trimmed = output.trim();
    if looks_like_json(trimmed) {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            return json_dirs(value);
        }
    }
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !looks_like_dirs_header(l))
        .filter(|l| l.starts_with('/') || l.starts_with('~') || l.starts_with('.'))
        .map(PathBuf::from)
        .collect()
}

fn looks_like_dirs_header(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "dir" | "dirs" | "directory" | "directories" | "path" | "paths"
    )
}

fn json_dirs(value: Value) -> Vec<PathBuf> {
    match value {
        Value::Array(arr) => arr
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(PathBuf::from(s)),
                Value::Object(map) => {
                    first_string(&map, &["path", "dir", "directory"]).map(PathBuf::from)
                }
                _ => None,
            })
            .collect(),
        Value::Object(map) => first_string_list(&map, &["dirs", "directories", "paths"]),
        _ => Vec::new(),
    }
}

/// Parse a process cmdline; returns info if it looks like `amp --no-tui`.
pub fn parse_amp_cmdline(args: &[String]) -> Option<LocalProcess> {
    let bin = args.first()?;
    let name = Path::new(bin).file_name()?.to_string_lossy();
    if name != "amp" && name != "amp.exe" {
        return None;
    }
    if !args.iter().any(|a| a == "--no-tui") {
        return None;
    }
    Some(LocalProcess {
        pid: 0,
        runner_id: flag_value(args, "--runner-id"),
        cwd: None,
        extra_dirs: flag_values(args, "--dir"),
    })
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().cloned().filter(|s| !s.is_empty());
        }
        if let Some(rest) = arg.strip_prefix(&format!("{flag}=")) {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn flag_values(args: &[String], flag: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == flag {
            if let Some(v) = iter.next() {
                out.push(PathBuf::from(v));
            }
        } else if let Some(rest) = arg.strip_prefix(&format!("{flag}=")) {
            out.push(PathBuf::from(rest));
        }
    }
    out
}

/// Scan this machine for `amp --no-tui` processes.
pub fn scan_local_amp_processes() -> Vec<LocalProcess> {
    #[cfg(target_os = "linux")]
    {
        scan_linux_proc()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        scan_via_ps()
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn scan_linux_proc() -> Vec<LocalProcess> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let cmdline_path = entry.path().join("cmdline");
        let Ok(bytes) = fs::read(cmdline_path) else {
            continue;
        };
        let args = split_cmdline(&bytes);
        let Some(mut proc) = parse_amp_cmdline(&args) else {
            continue;
        };
        proc.pid = pid;
        proc.cwd = fs::read_link(entry.path().join("cwd")).ok();
        found.push(proc);
    }
    found
}

#[cfg(all(unix, not(target_os = "linux")))]
fn scan_via_ps() -> Vec<LocalProcess> {
    let output = Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .collect()
}

#[cfg(unix)]
#[allow(dead_code)]
fn parse_ps_line(line: &str) -> Option<LocalProcess> {
    let line = line.trim();
    let (pid_str, rest) = line.split_once(char::is_whitespace)?;
    let pid = pid_str.parse().ok()?;
    let args = shellish_split(rest);
    let mut proc = parse_amp_cmdline(&args)?;
    proc.pid = pid;
    Some(proc)
}

pub fn split_cmdline(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[allow(dead_code)]
fn shellish_split(line: &str) -> Vec<String> {
    line.split_whitespace().map(ToOwned::to_owned).collect()
}

/// Merge Amp's list with local PIDs. Amp list wins for IDs/dirs; local scan fills PIDs.
pub fn merge_runners(
    listed: Vec<Runner>,
    local: &[LocalProcess],
    spawned: &[LocalProcess],
) -> Vec<Runner> {
    let mut runners = listed;
    for extra in local.iter().chain(spawned) {
        if let Some(existing) = find_match(&mut runners, extra) {
            if existing.pid.is_none() {
                existing.pid = Some(extra.pid);
            }
            if existing.cwd.is_none() {
                existing.cwd = extra.cwd.clone();
            }
            for dir in &extra.extra_dirs {
                push_unique_dir(&mut existing.dirs, dir.clone());
            }
            if extra.cwd.is_some() {
                if let Some(cwd) = &extra.cwd {
                    push_unique_dir(&mut existing.dirs, cwd.clone());
                }
            }
            existing.from_local_scan = true;
        } else {
            let mut dirs = extra.extra_dirs.clone();
            if let Some(cwd) = &extra.cwd {
                push_unique_dir(&mut dirs, cwd.clone());
            }
            runners.push(Runner {
                id: extra.runner_id.clone().unwrap_or_default(),
                pid: Some(extra.pid),
                cwd: extra.cwd.clone(),
                dirs,
                from_amp_list: false,
                from_local_scan: true,
            });
        }
    }
    runners.sort_by(|a, b| a.display_id().cmp(b.display_id()));
    runners
}

fn find_match<'a>(runners: &'a mut [Runner], local: &LocalProcess) -> Option<&'a mut Runner> {
    let by_id = local
        .runner_id
        .as_deref()
        .and_then(|id| runners.iter().position(|r| r.id.eq_ignore_ascii_case(id)));
    if let Some(idx) = by_id {
        return Some(&mut runners[idx]);
    }
    let by_cwd = local.cwd.as_ref().and_then(|cwd| {
        runners
            .iter()
            .position(|r| r.cwd.as_ref() == Some(cwd) || r.dirs.iter().any(|d| d == cwd))
    });
    by_cwd.map(|idx| &mut runners[idx])
}

/// Stop a process. Sends SIGTERM, then SIGKILL if it is still alive.
pub fn stop_pid(pid: u32) -> Result<()> {
    if pid == 0 {
        bail!("refusing to stop pid 0");
    }
    #[cfg(unix)]
    {
        send_signal(pid, libc::SIGTERM);
        for _ in 0..25 {
            if !pid_alive(pid) {
                forget_spawn(pid);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        send_signal(pid, libc::SIGKILL);
        forget_spawn(pid);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .context("taskkill failed")?;
        if !status.success() {
            bail!("taskkill exited with {status}");
        }
        forget_spawn(pid);
        Ok(())
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as i32, sig);
        libc::kill(-(pid as i32), sig);
    }
}

pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

fn runner_log_path(runner_id: Option<&str>, cwd: &Path) -> PathBuf {
    let slug = runner_id
        .map(sanitize_slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sanitize_slug(&cwd.to_string_lossy()));
    state_dir().join("logs").join(format!("{slug}.log"))
}

fn sanitize_slug(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    slug.trim_matches('_').to_string()
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct SpawnRecord {
    pid: u32,
    runner_id: Option<String>,
    cwd: String,
}

fn spawn_registry_path() -> PathBuf {
    state_dir().join("spawned.json")
}

fn load_spawn_registry() -> Vec<LocalProcess> {
    let path = spawn_registry_path();
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(records) = serde_json::from_str::<Vec<SpawnRecord>>(&raw) else {
        return Vec::new();
    };
    records
        .into_iter()
        .filter(|r| pid_alive(r.pid))
        .map(|r| LocalProcess {
            pid: r.pid,
            runner_id: r.runner_id,
            cwd: Some(PathBuf::from(r.cwd)),
            extra_dirs: Vec::new(),
        })
        .collect()
}

fn record_spawn(pid: u32, runner_id: Option<&str>, cwd: &Path) {
    let mut records = load_spawn_raw();
    records.retain(|r| r.pid != pid && pid_alive(r.pid));
    records.push(SpawnRecord {
        pid,
        runner_id: runner_id.map(ToOwned::to_owned),
        cwd: cwd.display().to_string(),
    });
    save_spawn_raw(&records);
}

fn forget_spawn(pid: u32) {
    let mut records = load_spawn_raw();
    records.retain(|r| r.pid != pid);
    save_spawn_raw(&records);
}

fn load_spawn_raw() -> Vec<SpawnRecord> {
    let path = spawn_registry_path();
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_spawn_raw(records: &[SpawnRecord]) {
    let path = spawn_registry_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_string_pretty(records) {
        let _ = fs::write(path, raw);
    }
}

pub fn missing_amp_message(binary: &Path) -> String {
    format!(
        "Amp CLI not found on PATH (looked for `{}`).\n\
         \n\
         lazyamp wraps the Amp CLI and cannot start without it.\n\
         Install Amp: https://ampcode.com/docs/cli\n\
         Then confirm `amp --help` works in this shell.\n\
         Optional: set AMP_BIN to the full path of the amp binary.",
        binary.display()
    )
}

pub fn resolve_executable(name: &Path) -> Option<PathBuf> {
    if name.components().count() > 1 || name.is_absolute() {
        return is_executable(name).then(|| name.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = candidate.with_extension("exe");
            if is_executable(&exe) {
                return Some(exe);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Light hostname-style check for `--runner-id` (Amp requires a valid hostname).
pub fn is_plausible_runner_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || id.len() > 253 || id.starts_with('-') || id.ends_with('-') {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_args_minimal() {
        assert_eq!(
            start_runner_args(&StartOptions::default()),
            vec!["--no-tui"]
        );
    }

    #[test]
    fn start_args_full() {
        let opts = StartOptions {
            runner_id: Some("dev-box".into()),
            mode: Some("high".into()),
            log_level: Some("debug".into()),
            settings_file: Some("/tmp/s.json".into()),
            mcp_config: Some("/tmp/m.json".into()),
            visibility: Some("private".into()),
            remote_control_terminal: true,
            extra_dirs: vec![PathBuf::from("/work/a"), PathBuf::from("/work/b")],
            discover_dirs: true,
            amp_env: true,
        };
        assert_eq!(
            start_runner_args(&opts),
            vec![
                "--no-tui",
                "--runner-id",
                "dev-box",
                "--mode",
                "high",
                "--log-level",
                "debug",
                "--settings-file",
                "/tmp/s.json",
                "--mcp-config",
                "/tmp/m.json",
                "--visibility",
                "private",
                "--remote-control-terminal",
                "--discover-dirs",
                "--amp-env",
                "--dir",
                "/work/a",
                "--dir",
                "/work/b",
            ]
        );
    }

    #[test]
    fn start_args_skip_blank_optionals() {
        let opts = StartOptions {
            runner_id: Some("  ".into()),
            mode: Some(String::new()),
            ..StartOptions::default()
        };
        assert_eq!(start_runner_args(&opts), vec!["--no-tui"]);
    }

    #[test]
    fn dirs_and_update_args() {
        let p = Path::new("/tmp/proj");
        assert_eq!(
            dirs_list_args(Some("box")),
            vec!["runner", "dirs", "list", "--runner-id", "box"]
        );
        assert_eq!(
            dirs_add_args(Some("box"), p),
            vec!["runner", "dirs", "add", "--runner-id", "box", "/tmp/proj"]
        );
        assert_eq!(
            dirs_remove_args(None, p),
            vec!["runner", "dirs", "remove", "/tmp/proj"]
        );
        assert_eq!(update_args(), vec!["update"]);
        assert_eq!(list_runners_args(true), vec!["runner", "list", "--json"]);
        assert_eq!(list_runners_args(false), vec!["runner", "list"]);
    }

    #[test]
    fn parse_json_array() {
        let raw = r#"[{"id":"mac-mini","pid":1234,"dirs":["/a","/b"]}]"#;
        let runners = parse_runner_list(raw).unwrap();
        assert_eq!(runners.len(), 1);
        assert_eq!(runners[0].id, "mac-mini");
        assert_eq!(runners[0].pid, Some(1234));
        assert_eq!(
            runners[0].dirs,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[test]
    fn parse_json_wrapped_camel_case() {
        let raw = r#"{"runners":[{"runnerId":"box","processId":"99","directories":["/tmp"],"cwd":"/tmp"}]}"#;
        let runners = parse_runner_list(raw).unwrap();
        assert_eq!(runners[0].id, "box");
        assert_eq!(runners[0].pid, Some(99));
        assert_eq!(runners[0].cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn parse_text_table_with_pid() {
        let raw = "\
ID          PID     DIRECTORIES
mac-mini    1234    /home/a, /home/b
work        99      /src
";
        let runners = parse_runner_list(raw).unwrap();
        assert_eq!(runners.len(), 2);
        assert_eq!(runners[0].id, "mac-mini");
        assert_eq!(runners[0].pid, Some(1234));
        assert_eq!(
            runners[0].dirs,
            vec![PathBuf::from("/home/a"), PathBuf::from("/home/b")]
        );
        assert_eq!(runners[1].id, "work");
        assert_eq!(runners[1].pid, Some(99));
    }

    #[test]
    fn parse_text_indented() {
        let raw = "\
mac-mini (pid 4321)
  /Users/me/code/amp
  /Users/me/code/sandcastle
";
        let runners = parse_runner_list(raw).unwrap();
        assert_eq!(runners.len(), 1);
        assert_eq!(runners[0].id, "mac-mini");
        assert_eq!(runners[0].pid, Some(4321));
        assert_eq!(runners[0].dirs.len(), 2);
    }

    #[test]
    fn parse_empty_list() {
        assert!(parse_runner_list("").unwrap().is_empty());
        assert!(parse_runner_list("   \n").unwrap().is_empty());
    }

    #[test]
    fn parse_dirs_list_paths_and_json() {
        let text = "directories\n/tmp/a\n/tmp/b\n";
        assert_eq!(
            parse_dirs_list(text),
            vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        let json = r#"["/x","/y"]"#;
        assert_eq!(
            parse_dirs_list(json),
            vec![PathBuf::from("/x"), PathBuf::from("/y")]
        );
    }

    #[test]
    fn cmdline_detects_no_tui_and_flags() {
        let args = vec![
            "/usr/local/bin/amp".into(),
            "--no-tui".into(),
            "--runner-id".into(),
            "box".into(),
            "--dir".into(),
            "/extra".into(),
        ];
        let proc = parse_amp_cmdline(&args).unwrap();
        assert_eq!(proc.runner_id.as_deref(), Some("box"));
        assert_eq!(proc.extra_dirs, vec![PathBuf::from("/extra")]);
        assert!(parse_amp_cmdline(&["amp".into(), "threads".into()]).is_none());
    }

    #[test]
    fn split_cmdline_null_separated() {
        let bytes = b"amp\0--no-tui\0--runner-id\0box\0";
        assert_eq!(
            split_cmdline(bytes),
            vec!["amp", "--no-tui", "--runner-id", "box"]
        );
    }

    #[test]
    fn merge_attaches_pid_by_id() {
        let listed = vec![Runner {
            id: "Box".into(),
            pid: None,
            cwd: None,
            dirs: vec![PathBuf::from("/work")],
            from_amp_list: true,
            from_local_scan: false,
        }];
        let local = vec![LocalProcess {
            pid: 7,
            runner_id: Some("box".into()),
            cwd: Some(PathBuf::from("/work")),
            extra_dirs: Vec::new(),
        }];
        let merged = merge_runners(listed, &local, &[]);
        assert_eq!(merged[0].pid, Some(7));
        assert!(merged[0].from_local_scan);
        assert!(merged[0].from_amp_list);
    }

    #[test]
    fn merge_adds_local_only_process() {
        let local = vec![LocalProcess {
            pid: 8,
            runner_id: Some("solo".into()),
            cwd: Some(PathBuf::from("/tmp")),
            extra_dirs: Vec::new(),
        }];
        let merged = merge_runners(Vec::new(), &local, &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "solo");
        assert!(!merged[0].from_amp_list);
    }

    #[test]
    fn runner_id_validation() {
        assert!(is_plausible_runner_id("dev-box"));
        assert!(is_plausible_runner_id("grandmas-garage-server"));
        assert!(!is_plausible_runner_id(""));
        assert!(!is_plausible_runner_id("-bad"));
        assert!(!is_plausible_runner_id("has space"));
    }

    #[test]
    fn resolve_executable_finds_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("amp");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = fs::metadata(&bin).unwrap().permissions();
            p.set_mode(0o755);
            fs::set_permissions(&bin, p).unwrap();
        }
        assert_eq!(resolve_executable(&bin), Some(bin));
    }
}
