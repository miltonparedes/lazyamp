//! Directory picker search: immediate browse, recursive filter, optional zoxide.

use crate::amp::resolve_executable;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Immediate children shown while navigating with hjkl.
pub const BROWSE_CHILD_CAP: usize = 400;
/// Cap on filter matches so the list stays usable.
pub const FILTER_RESULT_CAP: usize = 64;
const WALK_VISIT_CAP: usize = 2500;
const WALK_DEPTH_CAP: usize = 8;
const WALK_TIME_BUDGET: Duration = Duration::from_millis(400);
const ZOXIDE_TIMEOUT: Duration = Duration::from_millis(400);

/// Directory names that are expensive or uninteresting to walk.
const HEAVY_DIR_NAMES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "coverage",
    "__pycache__",
    "venv",
    ".venv",
    "site-packages",
    ".git",
    ".hg",
    ".svn",
    ".cache",
    ".next",
    ".turbo",
    ".direnv",
    ".yarn",
    ".pnpm-store",
    ".idea",
    ".vscode",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".terraform",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    Current,
    Home,
    Recent,
    Zoxide,
    Parent,
    Browse,
    Nested,
    Exact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedPath {
    pub source: PathSource,
    pub path: PathBuf,
}

/// True when `query` is a substring or a subsequence of `candidate`.
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

pub fn skip_dir_name(name: &str) -> bool {
    if name == "." || name == ".." {
        return true;
    }
    if name.starts_with('.') {
        return true;
    }
    HEAVY_DIR_NAMES
        .iter()
        .any(|heavy| name.eq_ignore_ascii_case(heavy))
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Immediate non-hidden children of `root` (hjkl browse).
pub fn list_immediate_subdirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(rd) = fs::read_dir(root) else {
        return dirs;
    };
    for entry in rd.flatten() {
        if dirs.len() >= BROWSE_CHILD_CAP {
            break;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        if skip_dir_name(&dir_name(&path)) {
            continue;
        }
        dirs.push(path);
    }
    dirs.sort();
    dirs
}

/// Recursively find directories under `root` whose path or name matches `query`.
pub fn walk_matching_dirs(root: &Path, query: &str) -> Vec<PathBuf> {
    walk_matching_dirs_limited(
        root,
        query,
        FILTER_RESULT_CAP,
        WALK_VISIT_CAP,
        WALK_TIME_BUDGET,
    )
}

pub fn walk_matching_dirs_limited(
    root: &Path,
    query: &str,
    max_results: usize,
    max_visit: usize,
    time_budget: Duration,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if query.trim().is_empty() {
        return out;
    }
    let deadline = Instant::now() + time_budget;
    let mut visited = 0usize;
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if visited >= max_visit || out.len() >= max_results || Instant::now() >= deadline {
            break;
        }
        visited += 1;
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let mut children = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            if skip_dir_name(&dir_name(&path)) {
                continue;
            }
            children.push(path);
        }
        children.sort();
        for child in children {
            if path_matches_query(query, &child) && out.len() < max_results {
                out.push(child.clone());
            }
            if depth + 1 < WALK_DEPTH_CAP {
                queue.push_back((child, depth + 1));
            }
        }
    }
    out
}

fn path_matches_query(query: &str, path: &Path) -> bool {
    let rendered = path.to_string_lossy();
    fuzzy_match(query, &rendered) || fuzzy_match(query, &dir_name(path))
}

/// Parse `zoxide query -l` stdout (one path per line, frecency-first).
pub fn parse_zoxide_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Run `zoxide query -l` when `zoxide` is on PATH. Empty if missing or slow.
pub fn query_zoxide_dirs() -> Vec<PathBuf> {
    if resolve_executable(Path::new("zoxide")).is_none() {
        return Vec::new();
    }
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("lazyamp-zoxide".into())
        .spawn(move || {
            let out = Command::new("zoxide").args(["query", "-l"]).output();
            let _ = tx.send(out);
        })
        .ok();
    match rx.recv_timeout(ZOXIDE_TIMEOUT) {
        Ok(Ok(output)) if output.status.success() => {
            parse_zoxide_output(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Vec::new(),
    }
}

fn matches_query(query: &str, path: &Path) -> bool {
    query.is_empty() || path_matches_query(query, path)
}

/// Inputs for a ranked picker list.
pub struct RankedSearch<'a> {
    pub query: &'a str,
    pub browse_root: &'a Path,
    pub recent_dirs: &'a [String],
    pub zoxide_dirs: &'a [PathBuf],
    pub nested_dirs: &'a [PathBuf],
    pub cwd: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub typed_exact: Option<&'a Path>,
    pub browse_children: bool,
}

/// Build a ranked, de-duplicated list of picker paths.
///
/// Filter mode prefers zoxide (anywhere), then recent dirs, then nested matches
/// under `browse_root`. Browse mode lists immediate children only.
pub fn collect_ranked_paths(search: RankedSearch<'_>) -> Vec<RankedPath> {
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let query = search.query.trim();

    let mut push = |source: PathSource, path: PathBuf| {
        if !path.as_os_str().is_empty() && seen.insert(path.clone()) {
            out.push(RankedPath { source, path });
        }
    };

    if let Some(path) = search.typed_exact {
        if path.is_dir() {
            push(PathSource::Exact, path.to_path_buf());
        }
    }
    for path in search.zoxide_dirs {
        if matches_query(query, path) {
            push(PathSource::Zoxide, path.clone());
        }
    }
    if let Some(cwd) = search.cwd {
        if matches_query(query, cwd) {
            push(PathSource::Current, cwd.to_path_buf());
        }
    }
    if let Some(home) = search.home {
        if matches_query(query, home) {
            push(PathSource::Home, home.to_path_buf());
        }
    }
    for recent in search.recent_dirs {
        let path = PathBuf::from(recent.trim());
        if path.as_os_str().is_empty() {
            continue;
        }
        if matches_query(query, &path) {
            push(PathSource::Recent, path);
        }
    }
    if query.is_empty() {
        if let Some(parent) = search.browse_root.parent() {
            push(PathSource::Parent, parent.to_path_buf());
        }
    }
    if search.browse_children {
        for child in list_immediate_subdirs(search.browse_root) {
            if matches_query(query, &child) {
                push(PathSource::Browse, child);
            }
        }
    }
    for path in search.nested_dirs {
        if matches_query(query, path) {
            push(PathSource::Nested, path.clone());
        }
    }

    if !query.is_empty() {
        out.truncate(FILTER_RESULT_CAP);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch_dir(path: &Path) {
        fs::create_dir_all(path).unwrap();
    }

    #[test]
    fn fuzzy_contains_and_subsequence() {
        assert!(fuzzy_match("amp", "/home/me/amp-cli"));
        assert!(fuzzy_match("hml", "/home/me/lazy"));
        assert!(!fuzzy_match("zzz", "/home/me"));
        assert!(fuzzy_match("", "anything"));
    }

    #[test]
    fn skip_heavy_and_hidden() {
        assert!(skip_dir_name("node_modules"));
        assert!(skip_dir_name("target"));
        assert!(skip_dir_name(".git"));
        assert!(skip_dir_name(".cache"));
        assert!(!skip_dir_name("src"));
        assert!(!skip_dir_name("amp"));
    }

    #[test]
    fn walk_finds_nested_and_skips_heavy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_dir(&root.join("code/amp/cli"));
        touch_dir(&root.join("code/sandcastle"));
        touch_dir(&root.join("code/node_modules/should-not-see"));
        touch_dir(&root.join("code/target/debug"));
        touch_dir(&root.join("code/.git/objects"));

        let nested = walk_matching_dirs_limited(root, "cli", 32, 200, Duration::from_secs(2));
        assert!(
            nested.iter().any(|p| p.ends_with("code/amp/cli")),
            "expected nested cli dir, got {nested:?}"
        );
        assert!(nested.iter().all(|p| {
            !p.components().any(|c| {
                let n = c.as_os_str().to_string_lossy();
                n == "node_modules" || n == "target" || n == ".git"
            })
        }));
    }

    #[test]
    fn zoxide_output_is_one_path_per_line() {
        let parsed = parse_zoxide_output("/home/me/code\n/tmp/work\n\n");
        assert_eq!(
            parsed,
            vec![PathBuf::from("/home/me/code"), PathBuf::from("/tmp/work")]
        );
    }

    #[test]
    fn ranked_filter_prefers_zoxide_then_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let zox = root.join("from-zoxide/deep/project");
        let recent = root.join("recent-only/project");
        let nested = root.join("browse/nested/project");
        touch_dir(&zox);
        touch_dir(&recent);
        touch_dir(&nested);

        let ranked = collect_ranked_paths(RankedSearch {
            query: "proj",
            browse_root: root,
            recent_dirs: &[recent.display().to_string()],
            zoxide_dirs: std::slice::from_ref(&zox),
            nested_dirs: std::slice::from_ref(&nested),
            cwd: None,
            home: None,
            typed_exact: None,
            browse_children: false,
        });
        let sources: Vec<PathSource> = ranked.iter().map(|r| r.source).collect();
        assert_eq!(sources.first().copied(), Some(PathSource::Zoxide));
        assert!(sources.contains(&PathSource::Recent));
        assert!(sources.contains(&PathSource::Nested));
        assert_eq!(ranked[0].path, zox);
    }

    #[test]
    fn browse_mode_lists_only_immediate_children() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_dir(&root.join("top"));
        touch_dir(&root.join("top/nested"));
        let ranked = collect_ranked_paths(RankedSearch {
            query: "",
            browse_root: root,
            recent_dirs: &[],
            zoxide_dirs: &[],
            nested_dirs: &[],
            cwd: None,
            home: None,
            typed_exact: None,
            browse_children: true,
        });
        assert!(ranked.iter().any(|r| r.path.ends_with("top")));
        assert!(!ranked.iter().any(|r| r.path.ends_with("nested")));
    }
}
