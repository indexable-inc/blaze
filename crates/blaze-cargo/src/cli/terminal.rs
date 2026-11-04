//! What stderr's terminal does, worked out once a command for the one renderer
//! ([`blaze_report::terminal`]): from `[terminal]` of blaze's `config.toml`, the file the
//! launcher's `[update]` is in (`$BLAZE_HOME/config.toml`, else `~/.blaze/config.toml`), and the
//! environment. The driver of [`blaze_report::terminal::detect`], which is pure: this reads the
//! file, the variables and whether stderr is a terminal.

use std::{
    io::IsTerminal as _,
    path::{Path, PathBuf},
};

use blaze_report::terminal::{Color, Config, Terminal};

/// blaze's `config.toml`: under `$BLAZE_HOME`, else `~/.blaze`; `None` when neither says.
pub fn config_file(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    match var("BLAZE_HOME").filter(|it| !it.is_empty()) {
        Some(home) => Some(Path::new(&home).join("config.toml")),
        None => var("HOME").filter(|it| !it.is_empty()).map(|home| Path::new(&home).join(".blaze").join("config.toml")),
    }
}

/// `[terminal]` of blaze's `config.toml`, its defaults where there is no file. A file that can't
/// be read, or that says what `[terminal]` does not take, is an error naming it.
pub fn config() -> Result<Config, String> {
    let Some(file) = config_file(|name| std::env::var(name).ok()) else { return Ok(Config::DEFAULT) };
    match std::fs::read_to_string(&file) {
        Ok(text) => Config::from_toml(&text).map_err(|why| format!("{}: {why}", file.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::DEFAULT),
        Err(error) => Err(format!("{} can't be read: {error}", file.display())),
    }
}

/// The `--color` a command takes from `config` where it gives none and `CARGO_TERM_COLOR` says
/// nothing: `--color`, then `CARGO_TERM_COLOR`, then `[terminal] color`.
pub fn color_flag<'a>(flag: Option<&'a str>, config: &Config, var: impl Fn(&str) -> Option<String>) -> Option<&'a str> {
    if flag.is_some() || var("CARGO_TERM_COLOR").is_some() {
        return flag;
    }
    match config.color {
        Color::Always => Some("always"),
        Color::Never => Some("never"),
        Color::Auto => None,
    }
}

/// What stderr's terminal does under `config`, its colour as the command decided it (`color`).
pub fn stderr(config: &Config, color: bool) -> Terminal {
    let terminal = std::io::stderr().is_terminal();
    let mut it = blaze_report::terminal::detect(config, |name| std::env::var(name).ok(), terminal, super::place::stderr_columns());
    it.caps.color = color;
    it
}
