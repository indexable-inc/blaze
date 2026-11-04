//! What the terminal can do, from its environment and `[terminal]` of blaze's `config.toml`
//! (under `BLAZE_HOME`, else `~/.blaze`: the file `[update]` is in), and the escape sequences
//! for each: colour and OSC 8 hyperlinks ([`crate::Caps`]), a native progress bar (OSC 9;4)
//! and a notification when a command ends (OSC 9, OSC 777, kitty's OSC 99).
//!
//! Pure: [`detect`] takes the variables as a function and whether the stream is a terminal,
//! so a test gives each terminal's environment; the caller reads the file and the process's
//! environment. Nothing is emitted that is not a terminal, under CI or with `TERM=dumb`; inside
//! tmux or screen, a progress bar or notification goes through tmux's passthrough only when
//! `[terminal] tmux_passthrough = true` says tmux allows it, else not at all.
//!
//! ```toml
//! [terminal]
//! color = "auto"          # "always", "never"; NO_COLOR is honoured under "auto"
//! hyperlinks = "auto"     # "off"
//! progress = "auto"       # "off"
//! notify = "failure"      # "always", "off"
//! notify_after_secs = 10  # "always" notifies a success only after this long
//! tmux_passthrough = false
//! ```

use crate::Caps;

/// `auto` or `off`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Auto {
    Auto,
    Off,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Auto,
    Always,
    Never,
}

/// When a command's end is a notification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notify {
    /// Only a failure: the default, since whether the terminal has focus can't be known.
    Failure,
    /// A failure, and a success that took [`Config::notify_after_secs`] or more.
    Always,
    Off,
}

/// `[terminal]` of `config.toml`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    pub color: Color,
    pub notify: Notify,
    pub progress: Auto,
    pub hyperlinks: Auto,
    pub notify_after_secs: u64,
    pub tmux_passthrough: bool,
}

impl Config {
    pub const DEFAULT: Config = Config {
        color: Color::Auto,
        notify: Notify::Failure,
        progress: Auto::Auto,
        hyperlinks: Auto::Auto,
        notify_after_secs: 10,
        tmux_passthrough: false,
    };

    /// `[terminal]` of `text`, a `config.toml`; the defaults for what it does not set. An
    /// error names the key and what it takes.
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let table: toml::Table =
            toml::from_str(text).map_err(|error| format!("config.toml is not TOML: {error}"))?;
        let mut config = Config::DEFAULT;
        let Some(terminal) = table.get("terminal") else { return Ok(config) };
        let Some(terminal) = terminal.as_table() else {
            return Err("config.toml's `terminal` is not a table ([terminal])".to_owned()); // alloc: an error, once
        };
        for (key, value) in terminal {
            // alloc: an error, once
            let bad = |wants: &str| {
                format!("config.toml's `terminal.{key}` takes {wants}, not `{value}`")
            };
            let word = || value.as_str();
            match key.as_str() {
                "color" => {
                    config.color = match word() {
                        Some("auto") => Color::Auto,
                        Some("always") => Color::Always,
                        Some("never") => Color::Never,
                        _ => return Err(bad("\"auto\", \"always\" or \"never\"")),
                    }
                }
                "hyperlinks" | "progress" => {
                    let it = match word() {
                        Some("auto") => Auto::Auto,
                        Some("off") => Auto::Off,
                        _ => return Err(bad("\"auto\" or \"off\"")),
                    };
                    match key.as_str() {
                        "hyperlinks" => config.hyperlinks = it,
                        _ => config.progress = it,
                    }
                }
                "notify" => {
                    config.notify = match word() {
                        Some("failure") => Notify::Failure,
                        Some("always") => Notify::Always,
                        Some("off") => Notify::Off,
                        _ => return Err(bad("\"failure\", \"always\" or \"off\"")),
                    }
                }
                "notify_after_secs" => {
                    config.notify_after_secs = value
                        .as_integer()
                        .and_then(|it| u64::try_from(it).ok())
                        .ok_or_else(|| bad("a number of seconds"))?
                }
                "tmux_passthrough" => {
                    config.tmux_passthrough = value.as_bool().ok_or_else(|| bad("true or false"))?
                }
                _ => {
                    // alloc: an error, once
                    return Err(format!(
                        "config.toml's `terminal` has no `{key}`: it takes color, hyperlinks, progress, notify, \
                         notify_after_secs and tmux_passthrough"
                    ));
                }
            }
        }
        Ok(config)
    }
}

/// How a notification is said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notifier {
    /// `ESC ] 9 ; text BEL`: iTerm2, Ghostty, WezTerm.
    Osc9,
    /// `ESC ] 777 ; notify ; title ; body BEL`: Ghostty, WezTerm, foot, urxvt.
    Osc777,
    /// kitty's `ESC ] 99 ; ... ST`.
    Kitty,
}

/// What this terminal does, worked out once a command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Terminal {
    pub caps: Caps,
    /// OSC 9;4 progress is shown.
    pub progress: bool,
    pub notifier: Option<Notifier>,
    pub notify: Notify,
    pub notify_after_secs: u64,
    /// Sequences other than colour go through tmux's passthrough.
    pub tmux: bool,
}

impl Terminal {
    /// A pipe, a file, CI: no escape of any kind.
    pub const PIPE: Terminal = Terminal {
        caps: Caps::PLAIN,
        progress: false,
        notifier: None,
        notify: Notify::Failure,
        notify_after_secs: 10,
        tmux: false,
    };
}

impl Default for Terminal {
    fn default() -> Terminal {
        Terminal::PIPE
    }
}

/// What the terminal of a stream does, from `config`, the variables `var` gives, whether the
/// stream is a terminal, and its width.
pub fn detect(
    config: &Config,
    var: impl Fn(&str) -> Option<String>,
    terminal: bool,
    width: Option<u16>,
) -> Terminal {
    let set = |name: &str| var(name).is_some_and(|it| !it.is_empty());
    let term = var("TERM").unwrap_or_default();
    let program = var("TERM_PROGRAM").unwrap_or_default();
    let dumb = term == "dumb";
    let ci = set("CI");
    let color = match config.color {
        Color::Always => true,
        Color::Never => false,
        Color::Auto if set("NO_COLOR") => false,
        Color::Auto if var("CLICOLOR_FORCE").is_some_and(|it| it != "0") => true,
        Color::Auto => terminal && !dumb,
    };
    let none = Terminal {
        caps: Caps { color, links: false, width: if terminal { width } else { None } },
        progress: false,
        notifier: None,
        notify: config.notify,
        notify_after_secs: config.notify_after_secs,
        tmux: false,
    };
    if !terminal || ci || dumb {
        return none;
    }
    let muxed = set("TMUX") || term.starts_with("screen") || term.starts_with("tmux");
    let kind = Kind::of(
        &program,
        &term,
        set("WT_SESSION"),
        set("KITTY_WINDOW_ID"),
        var("ConEmuANSI").as_deref() == Some("ON"),
    );
    let version = var("TERM_PROGRAM_VERSION").unwrap_or_default();
    // Ghostty draws OSC 9;4 since 1.2.
    let ghostty_progress = kind == Kind::Ghostty && at_least(&version, 1, 2);
    let shows_progress =
        ghostty_progress || matches!(kind, Kind::WezTerm | Kind::WindowsTerminal | Kind::ConEmu);
    let notifier = match kind {
        Kind::Ghostty | Kind::WezTerm | Kind::Foot => Some(Notifier::Osc777),
        Kind::ITerm => Some(Notifier::Osc9),
        Kind::Kitty => Some(Notifier::Kitty),
        Kind::VsCode | Kind::WindowsTerminal | Kind::ConEmu | Kind::Other => None,
    };
    let links = !matches!(kind, Kind::ConEmu | Kind::Other) && config.hyperlinks == Auto::Auto;
    let through = !muxed || config.tmux_passthrough;
    Terminal {
        caps: Caps { color, links: links && !muxed, width },
        progress: shows_progress && config.progress == Auto::Auto && through,
        notifier: notifier.filter(|_| through && config.notify != Notify::Off),
        notify: config.notify,
        notify_after_secs: config.notify_after_secs,
        tmux: muxed && config.tmux_passthrough,
    }
}

/// The terminal programs told apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Ghostty,
    ITerm,
    WezTerm,
    Kitty,
    Foot,
    VsCode,
    WindowsTerminal,
    ConEmu,
    Other,
}

impl Kind {
    fn of(program: &str, term: &str, windows_terminal: bool, kitty: bool, conemu: bool) -> Kind {
        match (program, term) {
            ("ghostty", _) | (_, "xterm-ghostty") => Kind::Ghostty,
            ("iTerm.app", _) => Kind::ITerm,
            ("WezTerm", _) => Kind::WezTerm,
            ("vscode", _) => Kind::VsCode,
            (_, "xterm-kitty") => Kind::Kitty,
            (_, "foot" | "foot-extra") => Kind::Foot,
            _ if kitty => Kind::Kitty,
            _ if windows_terminal => Kind::WindowsTerminal,
            _ if conemu => Kind::ConEmu,
            _ => Kind::Other,
        }
    }
}

/// Whether `version` (`1.2.0`) is at least `major.minor`.
fn at_least(version: &str, major: u32, minor: u32) -> bool {
    let mut parts = version.split('.').map(|it| it.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(a), Some(b)) => (a, b) >= (major, minor),
        (Some(a), None) => a > major,
        _ => false,
    }
}

/// A progress bar's state (OSC 9;4's `st`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress {
    /// No bar: what every command that showed one ends with, a failure and an interrupt too.
    Clear,
    /// `percent` done.
    Normal(u8),
    /// Failed, at `percent`.
    Error(u8),
    /// Under way, how far unknown.
    Indeterminate,
    Paused(u8),
}

impl Terminal {
    /// The sequence that shows `progress`, at the end of `out`; nothing where the terminal shows
    /// no bar. The caller limits how often (a few a second at most).
    pub fn progress_into(&self, out: &mut String, progress: Progress) {
        if !self.progress {
            return;
        }
        use std::fmt::Write as _;
        let (state, percent) = match progress {
            Progress::Clear => (0, 0),
            Progress::Normal(it) => (1, it.min(100)),
            Progress::Error(it) => (2, it.min(100)),
            Progress::Indeterminate => (3, 0),
            Progress::Paused(it) => (4, it.min(100)),
        };
        self.wrapped(out, |out| {
            // Writing to a String cannot fail.
            let _ = write!(out, "\x1b]9;4;{state};{percent}\x07");
        });
    }

    /// Whether a command that ended (`failed`, after `took_secs`) says so as a notification.
    pub fn notifies(&self, failed: bool, took_secs: u64) -> bool {
        self.notifier.is_some()
            && match self.notify {
                Notify::Off => false,
                Notify::Failure => failed,
                Notify::Always => failed || took_secs >= self.notify_after_secs,
            }
    }

    /// The notification `title`: `body`, at the end of `out`; nothing where the terminal has
    /// none. Control characters (and `;` where it separates fields) become spaces.
    pub fn notify_into(&self, out: &mut String, title: &str, body: &str) {
        let Some(notifier) = self.notifier else { return };
        self.wrapped(out, |out| match notifier {
            Notifier::Osc9 => {
                out.push_str("\x1b]9;");
                clean(out, title, false);
                out.push_str(": ");
                clean(out, body, false);
                out.push('\x07');
            }
            Notifier::Osc777 => {
                out.push_str("\x1b]777;notify;");
                clean(out, title, true);
                out.push(';');
                clean(out, body, false);
                out.push('\x07');
            }
            Notifier::Kitty => {
                out.push_str("\x1b]99;i=blaze:d=0;");
                clean(out, title, true);
                out.push_str("\x1b\\\x1b]99;i=blaze:d=1:p=body;");
                clean(out, body, true);
                out.push_str("\x1b\\");
            }
        });
    }

    /// What `write` writes, inside tmux's passthrough (`ESC P tmux; ... ESC \`, each escape
    /// doubled) when the terminal is behind tmux.
    fn wrapped(&self, out: &mut String, write: impl FnOnce(&mut String)) {
        if !self.tmux {
            return write(out);
        }
        let mut inner = String::new(); // alloc: a sequence, a few a second at most
        write(&mut inner);
        out.push_str("\x1bPtmux;");
        for c in inner.chars() {
            if c == '\x1b' {
                out.push('\x1b');
            }
            out.push(c);
        }
        out.push_str("\x1b\\");
    }
}

/// `text` with control characters, and `;` when `field`, as spaces.
fn clean(out: &mut String, text: &str, field: bool) {
    out.extend(text.chars().map(|c| if c.is_control() || (field && c == ';') { ' ' } else { c }));
}
