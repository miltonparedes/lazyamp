//! Self-update: resolve the latest GitHub release and reinstall via `install.sh`.
//!
//! Shells out to `curl` and `sh` (like `scripts/install.sh`) instead of pulling
//! in an HTTP client. The install script stages the new binary and renames it
//! over the old one, so replacing the running executable is safe.

use crate::config::{atomic_write, state_dir};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// GitHub `owner/name` that publishes lazyamp releases.
pub const REPO: &str = "miltonparedes/lazyamp";

const BINARY: &str = "lazyamp";
const CACHE_FILE: &str = "update-check.json";

/// Version of the running binary (without a leading `v`).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Resolve the latest release tag by following the `releases/latest` redirect.
pub fn latest_tag() -> Result<String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let output = Command::new("curl")
        .args(["-fsSL", "--connect-timeout", "10", "--max-time", "15"])
        .args(["-o", "/dev/null", "-w", "%{url_effective}"])
        .arg(&url)
        .stdin(Stdio::null())
        .output()
        .context("failed to run curl (is it installed?)")?;
    if !output.status.success() {
        bail!(
            "could not reach {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let effective = String::from_utf8_lossy(&output.stdout);
    let tag = effective.trim().trim_end_matches('/').rsplit('/').next();
    match tag {
        Some(tag) if !tag.is_empty() && tag != "latest" => Ok(tag.to_string()),
        _ => bail!("could not resolve the latest release from {url}"),
    }
}

/// Whether `tag` is a newer release than `current` (MAJOR.MINOR.PATCH, optional
/// leading `v`). Pre-releases sort before their release; unparseable → false.
pub fn is_newer(current: &str, tag: &str) -> bool {
    match (parse_version(current), parse_version(tag)) {
        (Some(current), Some(tag)) => tag > current,
        _ => false,
    }
}

/// `((major, minor, patch), is_release)`; releases sort after pre-releases.
fn parse_version(s: &str) -> Option<((u64, u64, u64), bool)> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let s = s.split_once('+').map_or(s, |(core, _build)| core);
    let (core, is_release) = match s.split_once('-') {
        Some((core, _pre)) => (core, false),
        None => (s, true),
    };
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let (major, minor, patch) = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    Some(((major, minor, patch), is_release))
}

/// Last resolved latest tag, persisted in the state dir.
#[derive(Debug, Serialize, Deserialize)]
struct UpdateCheck {
    tag: String,
    checked_at: u64,
}

fn cache_path() -> PathBuf {
    state_dir().join(CACHE_FILE)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Cached latest tag if it was resolved less than `max_age` ago.
pub fn cached_latest_tag(max_age: Duration) -> Option<String> {
    let raw = fs::read_to_string(cache_path()).ok()?;
    let cached: UpdateCheck = serde_json::from_str(&raw).ok()?;
    let age = now_unix().checked_sub(cached.checked_at)?;
    (age < max_age.as_secs() && !cached.tag.is_empty()).then_some(cached.tag)
}

/// Latest tag from the cache if fresh, otherwise resolve it and refresh the cache.
pub fn check_latest_cached(max_age: Duration) -> Result<String> {
    if let Some(tag) = cached_latest_tag(max_age) {
        return Ok(tag);
    }
    let tag = latest_tag()?;
    let record = UpdateCheck {
        tag: tag.clone(),
        checked_at: now_unix(),
    };
    // Disposable cache: a failed write must not fail the check.
    if let Ok(json) = serde_json::to_string(&record) {
        let _ = atomic_write(&cache_path(), json);
    }
    Ok(tag)
}

/// Resolve the running executable, dropping Linux's ` (deleted)` suffix that
/// appears after the binary has been replaced on disk.
pub fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to locate the running executable")?;
    let exe = strip_deleted(&exe);
    fs::canonicalize(&exe).with_context(|| format!("failed to resolve {}", exe.display()))
}

fn strip_deleted(path: &Path) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
}

/// Result of a successful [`install`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// Installed release tag (with leading `v`).
    pub tag: String,
    /// Path of the replaced binary.
    pub path: PathBuf,
    /// Combined stdout + stderr of `install.sh`.
    pub output: String,
}

/// Where `install.sh` should put the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Location {
    /// `--prefix DIR` (binary lands in `DIR/bin`; supported by every install.sh).
    Prefix(PathBuf),
    /// `--bin-dir DIR` (only install.sh versions after v0.1.1).
    BinDir(PathBuf),
}

impl Location {
    fn args(&self) -> [&std::ffi::OsStr; 2] {
        match self {
            Location::Prefix(dir) => ["--prefix".as_ref(), dir.as_os_str()],
            Location::BinDir(dir) => ["--bin-dir".as_ref(), dir.as_os_str()],
        }
    }
}

/// Pick the install.sh location argument that reproduces `exe`'s path.
fn install_location(exe: &Path) -> Result<Location> {
    let parent = exe
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .with_context(|| format!("{} has no parent directory", exe.display()))?;
    if parent.file_name().is_some_and(|name| name == "bin") {
        if let Some(prefix) = parent.parent() {
            return Ok(Location::Prefix(prefix.to_path_buf()));
        }
    }
    Ok(Location::BinDir(parent.to_path_buf()))
}

fn normalize_tag(tag: &str) -> String {
    let tag = tag.trim();
    if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    }
}

fn ensure_writable(dir: &Path, exe: &Path) -> Result<()> {
    let probe = dir.join(format!(".{BINARY}.write-test.{}", std::process::id()));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(err) => bail!(
            "cannot replace {}: {} is not writable ({err}); re-run with sudo, \
             or reinstall with scripts/install.sh to a writable location",
            exe.display(),
            dir.display()
        ),
    }
}

/// Download the tag's `install.sh` into a fresh private (0700) temp directory,
/// so no other user can swap the script between download and execution.
/// The directory is removed when the returned guard drops.
fn download_script(tag: &str) -> Result<(tempfile::TempDir, PathBuf)> {
    let url = format!("https://raw.githubusercontent.com/{REPO}/{tag}/scripts/install.sh");
    let mut builder = tempfile::Builder::new();
    builder.prefix("lazyamp-update-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let dir = builder
        .tempdir()
        .context("failed to create a private temp directory")?;
    let script = dir.path().join("install.sh");
    let output = Command::new("curl")
        .args(["-fsSL", "--connect-timeout", "10", "--max-time", "60", "-o"])
        .arg(&script)
        .arg(&url)
        .stdin(Stdio::null())
        .output()
        .context("failed to run curl (is it installed?)")?;
    if !output.status.success() {
        bail!(
            "failed to download {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok((dir, script))
}

/// Install release `tag` (latest when `None`) over `exe` using that release's
/// `install.sh`, then verify the new binary reports the expected version.
pub fn install(exe: &Path, tag: Option<&str>) -> Result<InstallOutcome> {
    let exe = strip_deleted(exe);
    if exe.file_name().is_none_or(|name| name != BINARY) {
        bail!(
            "cannot self-update {}: install.sh always installs a file named `{BINARY}`; \
             rename the binary or reinstall with scripts/install.sh",
            exe.display()
        );
    }
    let location = install_location(&exe)?;
    let parent = exe.parent().unwrap_or_else(|| Path::new("."));
    ensure_writable(parent, &exe)?;

    let tag = match tag {
        Some(tag) => normalize_tag(tag),
        None => latest_tag()?,
    };
    let version = tag.trim_start_matches('v').to_string();

    let (script_dir, script) = download_script(&tag)?;
    let result = Command::new("sh")
        .arg(&script)
        .args(["--version", tag.as_str()])
        .args(location.args())
        // Explicit args must win over any inherited install.sh env.
        .env_remove("BIN_DIR")
        .env_remove("PREFIX")
        .env_remove("VERSION")
        .stdin(Stdio::null())
        .output()
        .context("failed to run sh")?;
    drop(script_dir);

    let mut output = String::from_utf8_lossy(&result.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&result.stderr));
    if !result.status.success() {
        let hint = if matches!(location, Location::BinDir(_)) && output.contains("--bin-dir") {
            format!(
                "\nThe {tag} install.sh predates --bin-dir, so it can only install into a \
                 directory named `bin`; move {} into a `bin` directory or install a newer release.",
                exe.display()
            )
        } else {
            String::new()
        };
        bail!(
            "install.sh {tag} failed ({}):\n{}{hint}",
            result.status,
            output.trim_end()
        );
    }

    let probe = Command::new(&exe)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run {} --version", exe.display()))?;
    let reported = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    let expected = format!("{BINARY} {version}");
    if reported != expected {
        bail!(
            "installed {} but it reports `{reported}` (expected `{expected}`); install.sh output:\n{}",
            exe.display(),
            output.trim_end()
        );
    }

    Ok(InstallOutcome {
        tag,
        path: exe,
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_numerically() {
        assert!(is_newer("0.1.9", "0.1.10"));
        assert!(!is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("0.1.1", "0.2.0"));
        assert!(is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn is_newer_ignores_v_prefix() {
        assert!(is_newer("0.1.1", "v0.1.2"));
        assert!(is_newer("v0.1.1", "0.1.2"));
        assert!(!is_newer("v0.1.2", "v0.1.2"));
        assert!(!is_newer("0.1.2", "v0.1.1"));
    }

    #[test]
    fn is_newer_equal_is_false() {
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.1", "v0.1.1"));
    }

    #[test]
    fn is_newer_prerelease_is_older_than_release() {
        assert!(is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "v0.2.0-rc.1"));
        assert!(is_newer("0.1.9", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0-rc.2"));
    }

    #[test]
    fn is_newer_garbage_is_false() {
        assert!(!is_newer("0.1.1", "latest"));
        assert!(!is_newer("garbage", "0.2.0"));
        assert!(!is_newer("0.1.1", ""));
        assert!(!is_newer("0.1.1", "0.2"));
        assert!(!is_newer("0.1.1", "0.2.0.1"));
        assert!(!is_newer("0.1.1", "v0.x.0"));
    }

    #[test]
    fn location_uses_prefix_for_bin_dirs() {
        assert_eq!(
            install_location(Path::new("/usr/local/bin/lazyamp")).unwrap(),
            Location::Prefix(PathBuf::from("/usr/local"))
        );
        assert_eq!(
            install_location(Path::new("/home/u/.local/bin/lazyamp")).unwrap(),
            Location::Prefix(PathBuf::from("/home/u/.local"))
        );
    }

    #[test]
    fn location_uses_bin_dir_otherwise() {
        assert_eq!(
            install_location(Path::new("/opt/tools/lazyamp")).unwrap(),
            Location::BinDir(PathBuf::from("/opt/tools"))
        );
        assert_eq!(
            install_location(Path::new("/home/u/binaries/lazyamp")).unwrap(),
            Location::BinDir(PathBuf::from("/home/u/binaries"))
        );
    }

    #[test]
    fn strip_deleted_suffix() {
        assert_eq!(
            strip_deleted(Path::new("/usr/bin/lazyamp (deleted)")),
            PathBuf::from("/usr/bin/lazyamp")
        );
        assert_eq!(
            strip_deleted(Path::new("/usr/bin/lazyamp")),
            PathBuf::from("/usr/bin/lazyamp")
        );
    }

    #[test]
    fn normalize_tag_adds_v() {
        assert_eq!(normalize_tag("0.1.1"), "v0.1.1");
        assert_eq!(normalize_tag("v0.1.1"), "v0.1.1");
    }
}
