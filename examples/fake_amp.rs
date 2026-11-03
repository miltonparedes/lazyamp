//! Stand-in for the Amp CLI used by `scripts/e2e.sh`, so lazyamp can be driven
//! end to end without network access or a login.
//!
//! Install it as an executable named `amp` (lazyamp identifies runners by
//! `argv[0]`). `amp --no-tui …` stays alive until signaled; `amp runner list`
//! reports every live `--no-tui` process started from this same binary.

use std::path::PathBuf;
use std::{env, fs, thread, time::Duration};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "--no-tui") {
        println!("fake amp runner: {}", args.join(" "));
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    match words.as_slice() {
        ["version"] | [] => println!("0.0.0-fake"),
        ["runner", "list"] => println!("{}", runner_list()),
        // `dirs list`, `update`, …: succeed with nothing to report.
        _ => {}
    }
}

/// JSON shaped like `amp runner list --json` from the real CLI.
fn runner_list() -> String {
    let me = env::current_exe().ok();
    let mut runners = Vec::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let proc_dir = entry.path();
        if fs::read_link(proc_dir.join("exe")).ok() != me {
            continue;
        }
        let Ok(raw) = fs::read(proc_dir.join("cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if !argv.iter().any(|a| a == "--no-tui") {
            continue;
        }
        let id = argv
            .iter()
            .position(|a| a == "--runner-id")
            .and_then(|i| argv.get(i + 1))
            .cloned()
            .unwrap_or_default();
        let cwd = fs::read_link(proc_dir.join("cwd")).unwrap_or_else(|_| PathBuf::from("/"));
        runners.push(format!(
            r#"{{"runnerId":{},"workingDirectory":{},"serveCwd":true,"directories":[]}}"#,
            serde_json::Value::from(id),
            serde_json::Value::from(cwd.to_string_lossy())
        ));
    }
    format!(r#"{{"runners":[{}]}}"#, runners.join(","))
}
