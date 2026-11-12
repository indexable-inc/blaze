//! What stderr's terminal does, worked out once a command for the one renderer
//! ([`blaze_report::terminal`]): from `[terminal]` of blaze's `config.toml`, the file the
//! launcher's `[update]` is in (found by `blaze_manifest::user_config`), and the environment.
//! The driver of [`blaze_report::terminal::detect`], which is pure: this reads the file, the
//! variables and whether stderr is a terminal.

use std::io::IsTerminal as _;

use blaze_report::terminal::{Config, Terminal};

/// `[terminal]` of blaze's `config.toml`, its defaults where there is no file. A file that can't
/// be read, or that says what `[terminal]` does not take, is an error naming it.
pub fn config() -> Result<Config, String> {
    let Some(home) = blaze_manifest::user_config::home() else {
        return Ok(Config::DEFAULT);
    };
    let file = blaze_manifest::user_config::file(&home);
    match std::fs::read_to_string(&file) {
        Ok(text) => Config::from_toml(&text).map_err(|why| format!("{}: {why}", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::DEFAULT),
        Err(error) => Err(format!("{} can't be read: {error}", file.display())),
    }
}

// The `--color` a command takes from `config` where it gives none and `CARGO_TERM_COLOR` says
// nothing: blaze-thin's, which the thin `blaze` asks too.
pub use blaze_thin::request::color_flag;

/// What stderr's terminal does under `config`, its colour as the command decided it (`color`).
pub fn stderr(config: &Config, color: bool) -> Terminal {
    let terminal = std::io::stderr().is_terminal();
    let mut it = blaze_report::terminal::detect(
        config,
        |name| std::env::var(name).ok(),
        terminal,
        super::place::stderr_columns(),
    );
    it.caps.color = color;
    it
}
