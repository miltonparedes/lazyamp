fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("-h") | Some("--help") => {
            print_help();
        }
        Some("-V") | Some("--version") => {
            println!("lazyamp {}", env!("CARGO_PKG_VERSION"));
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

fn print_help() {
    println!(
        "\
lazyamp {}
Terminal UI for managing Amp --no-tui runners.

USAGE:
    lazyamp
    lazyamp --help
    lazyamp --version

Amp must be on PATH (or set AMP_BIN to the amp binary).
Config: $XDG_CONFIG_HOME/lazyamp/config.toml
        (default ~/.config/lazyamp/config.toml)

This UI only manages runners. It does not replace Amp's interactive agent TUI.
",
        env!("CARGO_PKG_VERSION")
    );
}
