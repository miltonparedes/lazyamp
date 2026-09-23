//! Lazy TUI for managing Amp `--no-tui` runners.

pub mod amp;
pub mod config;
pub mod ui;

/// Launch the terminal UI. Fails if the Amp CLI is not on `PATH`.
pub fn run() -> anyhow::Result<()> {
    ui::run()
}
