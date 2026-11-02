//! cargo's own lines as diagnostics: what cargo writes of a unit (a build script's failure, its
//! `cargo::warning`s), not rustc, with no span, rendered as cargo's shell writes them
//! (`cargo/src/cargo/core/shell.rs`: the status bold and coloured, the colon bold).

#![forbid(unsafe_code)]

use blaze_api::{Diagnostic, Level, UnitOf};

/// cargo's `style::ERROR`: bright red, bold.
const ERROR: &str = "\x1b[1m\x1b[91m";
/// cargo's `style::WARN`: bright yellow, bold.
const WARN: &str = "\x1b[1m\x1b[93m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// A line cargo writes at `level` saying `message`, of `unit`.
pub(crate) fn cargo(level: Level, message: String, unit: Option<UnitOf>, color: bool) -> Diagnostic {
    let rendered = rendered(level, &message, color);
    Diagnostic {
        level,
        code: None,
        message,
        spans: Vec::new(),
        children: Vec::new(),
        facts: Vec::new(),
        rendered,
        unit,
        explanation: None,
    }
}

/// `error: message` as cargo's shell writes it, with its newline.
pub(crate) fn rendered(level: Level, message: &str, color: bool) -> String {
    let (status, style) = match level {
        Level::Error => ("error", ERROR),
        Level::Warning => ("warning", WARN),
        Level::Note => ("note", BOLD),
        Level::Help => ("help", BOLD),
        Level::FailureNote => {
            return match color {
                true => format!("{BOLD}{message}{RESET}\n"), // alloc: a diagnostic's text, once
                false => format!("{message}\n"), // alloc: as above
            };
        }
    };
    match color {
        true => format!("{style}{status}{RESET}{BOLD}:{RESET} {message}\n"),
        false => format!("{status}: {message}\n"),
    }
}

/// An error of blaze's that is not rustc's: the unit can't be made, and says why.
pub(crate) fn refused(what: String, unit: Option<UnitOf>, color: bool) -> Diagnostic {
    cargo(Level::Error, what, unit, color)
}
