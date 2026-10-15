use lazyamp::update;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("-h") | Some("--help") => {
            print_help();
        }
        Some("-V") | Some("--version") => {
            println!("lazyamp {}", env!("CARGO_PKG_VERSION"));
        }
        Some("update") => {
            let cmd = match parse_update_args(args) {
                Ok(cmd) => cmd,
                Err(msg) => {
                    eprintln!("lazyamp: {msg}");
                    eprintln!("Try `lazyamp --help`.");
                    std::process::exit(2);
                }
            };
            if let Err(err) = run_update(cmd) {
                eprintln!("lazyamp: {err:#}");
                std::process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("lazyamp: unknown argument `{other}`");
            eprintln!("Try `lazyamp --help`.");
            std::process::exit(2);
        }
        None => {
            if let Err(err) = lazyamp::run() {
                eprintln!("lazyamp: {err:#}");
                std::process::exit(1);
            }
        }
    }
}

/// What `lazyamp update` should do.
enum UpdateCmd {
    Help,
    /// Only report current vs latest.
    Check,
    /// Install the latest release if newer.
    Latest,
    /// Install this exact tag (may be older: rollback).
    Pinned(String),
}

fn parse_update_args(mut args: impl Iterator<Item = String>) -> Result<UpdateCmd, String> {
    let mut check = false;
    let mut version = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(UpdateCmd::Help),
            "--check" => check = true,
            "--version" => match args.next() {
                Some(tag) if !tag.is_empty() => version = Some(tag),
                _ => return Err("update --version requires a tag (e.g. v0.1.1)".into()),
            },
            other => match other.strip_prefix("--version=") {
                Some(tag) if !tag.is_empty() => version = Some(tag.to_string()),
                Some(_) => return Err("update --version requires a tag (e.g. v0.1.1)".into()),
                None => return Err(format!("unknown update argument `{other}`")),
            },
        }
    }
    match (check, version) {
        (true, Some(_)) => Err("update --check and --version cannot be combined".into()),
        (true, None) => Ok(UpdateCmd::Check),
        (false, Some(tag)) => Ok(UpdateCmd::Pinned(tag)),
        (false, None) => Ok(UpdateCmd::Latest),
    }
}

const RUNNERS_NOTE: &str = "Running Amp runners are separate processes and were not restarted; \
restart lazyamp to use the new version.";

fn run_update(cmd: UpdateCmd) -> anyhow::Result<()> {
    let current = update::current_version();
    let tag = match cmd {
        UpdateCmd::Help => {
            print_help();
            return Ok(());
        }
        UpdateCmd::Check => {
            // Zero max age: always resolve, but refresh the TUI's cache too.
            let latest = update::check_latest_cached(Duration::ZERO)?;
            println!("current: lazyamp {current}");
            println!("latest:  {latest}");
            if update::is_newer(current, &latest) {
                println!("An update is available. Run `lazyamp update` to install {latest}.");
            } else {
                println!("lazyamp {current} is up to date.");
            }
            return Ok(());
        }
        UpdateCmd::Latest => {
            let latest = update::check_latest_cached(Duration::ZERO)?;
            if !update::is_newer(current, &latest) {
                println!("lazyamp {current} is up to date (latest release: {latest}).");
                return Ok(());
            }
            latest
        }
        UpdateCmd::Pinned(tag) => tag,
    };

    let exe = update::current_exe()?;
    println!("Installing lazyamp {tag} over {} ...", exe.display());
    let outcome = update::install(&exe, Some(&tag))?;
    for line in outcome.output.lines() {
        println!("  {line}");
    }
    println!(
        "Updated lazyamp {current} -> {} at {}",
        outcome.tag,
        outcome.path.display()
    );
    println!("{RUNNERS_NOTE}");
    Ok(())
}

fn print_help() {
    println!(
        "\
lazyamp {}
Terminal UI for managing Amp --no-tui runners.

USAGE:
    lazyamp
    lazyamp update [--check | --version <TAG>]
    lazyamp --help
    lazyamp --version

UPDATE:
    lazyamp update                  Install the latest release if it is newer
    lazyamp update --check          Show current and latest versions; install nothing
    lazyamp update --version <TAG>  Install exactly <TAG> (e.g. v0.1.1; allows rollback)

    Replaces the running binary in place via the release's install.sh
    (needs curl and write access to its directory). Running Amp runners are
    separate processes and are not restarted.

Amp must be on PATH (or set AMP_BIN to the amp binary).
Config: $XDG_CONFIG_HOME/lazyamp/config.toml
        (default ~/.config/lazyamp/config.toml)

This UI only manages runners. It does not replace Amp's interactive agent TUI.
",
        env!("CARGO_PKG_VERSION")
    );
}
