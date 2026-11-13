//! The request of a plain `build` or `run`, as blaze's CLI sends it
//! (`blaze_cargo::cli::request`): the same value, so the same bytes in the API's
//! deterministic CBOR, which blaze-cargo's tests hold a table of command lines to. And the
//! rule of whether stderr is colored, which that request carries: blaze-cargo's `cli::place`
//! and `cli::terminal` take it from here.

use blaze_api::{Cargo, Request, Targets, ViewRef};
use blaze_report::terminal::{Color, Config};

use crate::route::Asked;

/// How many of the functions blaze could not compile a build's error names without
/// `--refused` (`blaze_cargo::REFUSED_SHOWN` is this).
pub const REFUSED_SHOWN: u32 = 16;

/// Whether what cargo writes to stderr is colored: `--color` (the last one), else
/// `CARGO_TERM_COLOR`, else `auto`, which is anstream's choice for a stream that is a
/// `terminal` or not (`NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM`, `CI`).
pub fn color(
    flag: Option<&str>,
    var: impl Fn(&str) -> Option<String>,
    terminal: bool,
) -> Result<bool, String> {
    let when = match flag {
        Some(flag) => Some(flag.to_owned()),
        None => var("CARGO_TERM_COLOR"),
    };
    match when.as_deref() {
        Some("always") => Ok(true),
        Some("never") => Ok(false),
        Some("auto") | None => Ok(auto(&var, terminal)),
        // cargo's `ColorChoice::from_str`, of `--color` (which clap took in any case) or
        // `term.color`.
        Some(other) => {
            Err(format!("argument for --color must be auto, always, or never, but found `{other}`"))
        }
    }
}

/// anstream's `AutoStream::choice`.
fn auto(var: &impl Fn(&str) -> Option<String>, terminal: bool) -> bool {
    let clicolor = var("CLICOLOR").map(|it| it != "0");
    if var("NO_COLOR").is_some_and(|it| !it.is_empty()) {
        return false;
    }
    if var("CLICOLOR_FORCE").is_some_and(|it| it != "0") {
        return true;
    }
    if clicolor == Some(false) {
        return false;
    }
    let term = cfg!(windows) || var("TERM").is_some_and(|it| it != "dumb");
    terminal && (term || clicolor == Some(true) || var("CI").is_some())
}

/// The `--color` a command takes from `config` where it gives none and `CARGO_TERM_COLOR` says
/// nothing: `--color`, then `CARGO_TERM_COLOR`, then `[terminal] color`.
pub fn color_flag<'a>(
    flag: Option<&'a str>,
    config: &Config,
    var: impl Fn(&str) -> Option<String>,
) -> Option<&'a str> {
    if flag.is_some() || var("CARGO_TERM_COLOR").is_some() {
        return flag;
    }
    match config.color {
        Color::Always => Some("always"),
        Color::Never => Some("never"),
        Color::Auto => None,
    }
}

/// What of the machine a command is sent with, each as the CLI reads it
/// (`blaze_cargo::cli::prepared_cargo`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sent {
    /// The directory the command ran in, absolute, symlinks resolved.
    pub cwd: String,
    /// The checkout: what the view is of.
    pub root: String,
    /// The whole environment, sorted by name.
    pub env: Vec<(String, String)>,
    /// cargo's config files outside the checkout, each with its bytes.
    pub files: Vec<(String, Vec<u8>)>,
    pub color: bool,
    /// The columns diagnostics are rendered in.
    pub width: Option<u32>,
}

/// The request `asked` is, sent with `sent`: a build of the head of the checkout, with the
/// whole command line. No flag the thin `blaze` routes ([`crate::route`]) names a package, a
/// feature, a target or a goal, so those of [`Targets`] are empty, and its profile is the
/// command's own (`dev`) or `--release`'s.
pub fn request(asked: &Asked, sent: Sent) -> Request {
    let Sent { cwd, root, env, files, color, width } = sent;
    let cargo = Cargo {
        command: asked.command.name().to_owned(), // alloc: the request's, once a command
        args: asked.args.clone(),                 // clone: the request owns its words
        root: root.clone(),                       // clone: the view names the root too
        cwd,
        env,
        files,
        color,
        width,
        // `-q` and no `-v`: cargo's `Verbosity` as a count is 0 either way.
        verbose: 0,
        refused: Some(REFUSED_SHOWN),
        // The thin `blaze` routes no `--no-prebuilt` ([`crate::route`]): the full CLI reads it.
        prebuilt: true,
    };
    let profile = if asked.release { "release" } else { "dev" };
    let targets = Targets {
        packages: Vec::new(), // alloc: none
        profile: profile.to_owned(), // alloc: the request's, once a command
        goal: None,
        target: None,
        features: Vec::new(), // alloc: none
        all_features: false,
        no_default_features: false,
        cargo: Some(cargo),
    };
    Request::Build { view: ViewRef::Head(root), targets }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(it, _)| *it == name).map(|(_, value)| (*value).to_owned())
    }

    #[test]
    fn color_is_the_flags_then_the_variables_then_the_terminals() {
        assert_eq!(color(Some("always"), var(&[]), false), Ok(true));
        assert_eq!(color(None, var(&[("CARGO_TERM_COLOR", "never")]), true), Ok(false));
        assert_eq!(color(None, var(&[("TERM", "xterm")]), true), Ok(true));
        assert_eq!(color(None, var(&[("TERM", "xterm")]), false), Ok(false));
        assert_eq!(color(None, var(&[("TERM", "xterm"), ("NO_COLOR", "1")]), true), Ok(false));
        assert_eq!(color(None, var(&[("CLICOLOR_FORCE", "1")]), false), Ok(true));
        assert!(color(Some("sometimes"), var(&[]), true).is_err());
    }

    #[test]
    fn the_configs_color_stands_only_where_nothing_else_says() {
        let always = Config { color: Color::Always, ..Config::DEFAULT };
        assert_eq!(color_flag(None, &always, var(&[])), Some("always"));
        assert_eq!(color_flag(Some("never"), &always, var(&[])), Some("never"));
        assert_eq!(color_flag(None, &always, var(&[("CARGO_TERM_COLOR", "auto")])), None);
        assert_eq!(color_flag(None, &Config::DEFAULT, var(&[])), None);
    }
}
