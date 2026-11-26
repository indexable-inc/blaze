//! Pure `#![no_std]`, zero-unsafe, sans-IO declarative CLI schema, argv parser, help/usage
//! renderer, and [`CliSpec`] bridge (Rule 30: one concept, one home; Rule 56: reflection-first).
//!
//! Zero proc macros, zero `std`/`alloc`, explicit [`blaze_own::Alloc`] for parsed results, and
//! zero-allocation [`core::fmt::Write`] rendering for help and usage text.

use core::fmt;

use blaze_own::{AVec, Alloc, String, Vec};

use crate::{CliSpec, FlagSpec, SpecSource, SubcommandSpec};

/// Arity and tokenization rule of one CLI flag ([`ArgSpec`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// Presence-only boolean flag (`--stats`, `--open`). Takes no value.
    Bool,
    /// Single-value flag (`--output <path>`, `-o <path>`, `--color=always`).
    Value,
    /// Repeatable value flag (`-p <pkg>`, `--grant <k=v>`). Values are kept in order.
    MultiValue,
    /// Consumes zero or more following tokens while `!word.starts_with("--")`
    /// (e.g. `--fix [selector...]`), recording the `(start, end)` index span into `argv`.
    RestUntilFlag,
}

/// Declarative specification of one CLI option/flag.
///
/// `long` omits the leading `--` (e.g. `Some("output")`); `short` omits the leading `-`
/// (e.g. `Some('o')`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgSpec {
    pub id: &'static str,
    pub long: Option<&'static str>,
    pub short: Option<char>,
    pub value_hint: Option<&'static str>,
    pub kind: ArgKind,
    pub about: &'static str,
    pub hidden: bool,
}

impl ArgSpec {
    /// Empty placeholder spec useful in `const` array initialization (e.g. comptime reflection).
    pub const EMPTY: ArgSpec = ArgSpec {
        id: "",
        long: None,
        short: None,
        value_hint: None,
        kind: ArgKind::Bool,
        about: "",
        hidden: false,
    };
}

/// Declarative specification of one CLI subcommand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub hidden: bool,
    pub about: &'static str,
    pub usage_args: &'static str,
    pub flags: &'static [ArgSpec],
    pub accepts_passthrough: bool,
}

/// A removed or renamed command refused with a clear replacement message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenamedCommand {
    pub old: &'static str,
    pub new_cmd: &'static str,
    pub message: &'static str,
}

/// Complete declarative specification of a CLI application: the single source of truth for
/// parsing, `--help`, usage lines, and shell completions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppSpec {
    pub name: &'static str,
    pub about: &'static str,
    pub commands: &'static [CommandSpec],
    pub common_flags: &'static [ArgSpec],
    pub examples: &'static [&'static str],
    pub renamed: &'static [RenamedCommand],
}

/// Top-level outcome of [`parse_argv`].
#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome<'arg, 'alloc> {
    /// Help requested for the top-level app (`None`) or a specific canonical command (`Some`).
    Help(Option<&'static str>),
    /// Version requested (`-V`, `--version`, or `version`).
    Version,
    /// A resolved subcommand with parsed flags, positionals, and optional `--` passthrough args.
    Command(ParsedCommand<'arg, 'alloc>),
}

/// One parsed flag occurrence (or coalesced occurrences of the same `id`).
///
/// - For [`ArgKind::Bool`]: `values` is empty and `span` is `None`.
/// - For [`ArgKind::Value`] / [`ArgKind::MultiValue`]: `values` holds each argument in order.
/// - For [`ArgKind::RestUntilFlag`]: `span` holds `Some((start_idx, end_idx))` into the original
///   `argv` slice, and `values` holds the consumed words.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedFlag<'arg, 'alloc> {
    pub id: &'static str,
    pub values: AVec<'alloc, &'arg str>,
    pub span: Option<(usize, usize)>,
}

/// Parsed invocation of one subcommand from [`parse_argv`].
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedCommand<'arg, 'alloc> {
    pub command: &'static str,
    pub flags: AVec<'alloc, ParsedFlag<'arg, 'alloc>>,
    pub positionals: AVec<'alloc, &'arg str>,
    pub passthrough: AVec<'alloc, &'arg str>,
}

impl<'arg, 'alloc> ParsedCommand<'arg, 'alloc> {
    /// Returns `true` if a flag with `id` was supplied.
    pub fn has(&self, id: &str) -> bool {
        self.flags.iter().any(|f| f.id == id)
    }

    /// Returns the last value supplied for `id`, if any.
    pub fn last_value(&self, id: &str) -> Option<&'arg str> {
        self.flags
            .iter()
            .find(|f| f.id == id)
            .and_then(|f| f.values.as_slice().last().copied())
    }

    /// Iterates over all values supplied for `id` in command-line order.
    pub fn values<'s>(&'s self, id: &str) -> impl Iterator<Item = &'arg str> + use<'s, 'arg> {
        let slice: &'s [&'arg str] = match self.flags.iter().find(|f| f.id == id) {
            Some(flag) => flag.values.as_slice(),
            None => &[],
        };
        slice.iter().copied()
    }

    /// Returns the `(start, end)` `argv` index span recorded for a [`ArgKind::RestUntilFlag`] flag.
    pub fn span(&self, id: &str) -> Option<(usize, usize)> {
        self.flags.iter().find(|f| f.id == id).and_then(|f| f.span)
    }
}

/// Typed CLI parse refusal naming the command, flag, or replacement (Rule 19).
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError<'a> {
    Renamed {
        old: &'static str,
        instead: &'static str,
    },
    UnknownCommand {
        got: &'a str,
    },
    UnknownFlag {
        command: &'static str,
        flag: &'a str,
    },
    WrongCommandFlag {
        flag: &'a str,
        command: &'static str,
        valid_for: &'static str,
    },
    MissingValue {
        flag: &'a str,
    },
    PassthroughNotAllowed {
        command: &'static str,
    },
}

impl fmt::Display for ParseError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Renamed { old, instead } => {
                write!(f, "no `{old}` command: {instead}")
            }
            ParseError::UnknownCommand { got } => write!(f, "unknown command `{got}`"),
            ParseError::UnknownFlag { command, flag } => {
                write!(f, "`{command}`: unknown flag `{flag}`")
            }
            ParseError::WrongCommandFlag {
                flag,
                command,
                valid_for,
            } => write!(f, "`{flag}` is `{valid_for}`'s flag, not `{command}`'s"),
            ParseError::MissingValue { flag } => write!(f, "`{flag}` requires a value"),
            ParseError::PassthroughNotAllowed { command } => {
                write!(f, "`{command}` does not accept `--` passthrough arguments")
            }
        }
    }
}

impl core::error::Error for ParseError<'_> {}

/// Look up a command in `app` by canonical name or any of its aliases.
pub fn find_command<'a>(app: &'a AppSpec, name: &str) -> Option<&'a CommandSpec> {
    app.commands
        .iter()
        .find(|cmd| cmd.name == name || cmd.aliases.iter().any(|&alias| alias == name))
}

/// Resolve a command name or alias to its canonical name.
pub fn resolve_command_name(app: &AppSpec, name: &str) -> Option<&'static str> {
    find_command(app, name).map(|cmd| cmd.name)
}

/// Parse an `argv` slice (with `argv[0]` as program name) against `app` into `alloc`.
pub fn parse_argv<'arg, 'alloc>(
    app: &AppSpec,
    argv: &[&'arg str],
    alloc: Alloc<'alloc>,
) -> Result<ParseOutcome<'arg, 'alloc>, ParseError<'arg>> {
    if argv.len() <= 1 {
        return Ok(ParseOutcome::Help(None));
    }

    let sub = argv[1];
    match sub {
        "-h" | "--help" => return Ok(ParseOutcome::Help(None)),
        "-V" | "--version" | "version" => return Ok(ParseOutcome::Version),
        "help" => {
            let target = argv
                .get(2)
                .and_then(|&name| resolve_command_name(app, name));
            return Ok(ParseOutcome::Help(target));
        }
        _ => {}
    }

    if let Some(renamed) = app.renamed.iter().find(|r| r.old == sub) {
        return Err(ParseError::Renamed {
            old: renamed.old,
            instead: renamed.message,
        });
    }

    let Some(cmd) = find_command(app, sub) else {
        return Err(ParseError::UnknownCommand { got: sub });
    };

    if !cmd.hidden
        && argv[2..]
            .iter()
            .take_while(|&&a| a != "--")
            .any(|&a| matches!(a, "-h" | "--help"))
    {
        return Ok(ParseOutcome::Help(Some(cmd.name)));
    }

    let mut state = ParseState {
        command: cmd.name,
        argv,
        at: 2,
        flags: Vec::new_in(alloc),
        alloc,
    };
    let mut positionals: AVec<'alloc, &'arg str> = Vec::new_in(alloc);
    let mut passthrough: AVec<'alloc, &'arg str> = Vec::new_in(alloc);

    while state.at < argv.len() {
        let word = argv[state.at];
        state.at += 1;

        if word == "--" {
            if !cmd.accepts_passthrough {
                return Err(ParseError::PassthroughNotAllowed { command: cmd.name });
            }
            for &extra in &argv[state.at..] {
                passthrough.push(extra);
            }
            break;
        }

        if let Some(long_rest) = word.strip_prefix("--") {
            let (flag_label, long_name, inline_val) = match long_rest.split_once('=') {
                Some((name, val)) => (&word[..2 + name.len()], name, Some(val)),
                None => (word, long_rest, None),
            };
            let Some(spec) = cmd.flags.iter().find(|f| f.long == Some(long_name)) else {
                if let Some(owner) = find_flag_owner(app, cmd.name, Some(long_name), None) {
                    return Err(ParseError::WrongCommandFlag {
                        flag: flag_label,
                        command: cmd.name,
                        valid_for: owner,
                    });
                }
                return Err(ParseError::UnknownFlag {
                    command: cmd.name,
                    flag: flag_label,
                });
            };
            state.consume_flag(spec, word, flag_label, inline_val)?;
            continue;
        }

        if word.len() > 1 && word.starts_with('-') {
            let after_dash = &word[1..];
            let mut chars = after_dash.chars();
            let Some(ch) = chars.next() else {
                positionals.push(word);
                continue;
            };
            let rem = chars.as_str();
            let (flag_label, inline_val) = if rem.is_empty() {
                (word, None)
            } else if let Some(val) = rem.strip_prefix('=') {
                (&word[..1 + ch.len_utf8()], Some(val))
            } else {
                return Err(ParseError::UnknownFlag {
                    command: cmd.name,
                    flag: word,
                });
            };
            let Some(spec) = cmd.flags.iter().find(|f| f.short == Some(ch)) else {
                if let Some(owner) = find_flag_owner(app, cmd.name, None, Some(ch)) {
                    return Err(ParseError::WrongCommandFlag {
                        flag: flag_label,
                        command: cmd.name,
                        valid_for: owner,
                    });
                }
                return Err(ParseError::UnknownFlag {
                    command: cmd.name,
                    flag: flag_label,
                });
            };
            state.consume_flag(spec, word, flag_label, inline_val)?;
            continue;
        }

        positionals.push(word);
    }

    Ok(ParseOutcome::Command(ParsedCommand {
        command: cmd.name,
        flags: state.flags,
        positionals,
        passthrough,
    }))
}

struct ParseState<'a, 'arg, 'alloc> {
    command: &'static str,
    argv: &'a [&'arg str],
    at: usize,
    flags: AVec<'alloc, ParsedFlag<'arg, 'alloc>>,
    alloc: Alloc<'alloc>,
}

impl<'arg, 'alloc> ParseState<'_, 'arg, 'alloc> {
    fn consume_flag(
        &mut self,
        spec: &ArgSpec,
        raw_word: &'arg str,
        flag_label: &'arg str,
        inline_val: Option<&'arg str>,
    ) -> Result<(), ParseError<'arg>> {
        match spec.kind {
            ArgKind::Bool => {
                if inline_val.is_some() {
                    return Err(ParseError::UnknownFlag {
                        command: self.command,
                        flag: raw_word,
                    });
                }
                let _ = get_or_insert_flag(&mut self.flags, spec.id, self.alloc);
                Ok(())
            }
            ArgKind::Value | ArgKind::MultiValue => {
                let val = match inline_val {
                    Some(v) => v,
                    None => {
                        let Some(&next) = self.argv.get(self.at) else {
                            return Err(ParseError::MissingValue { flag: flag_label });
                        };
                        self.at += 1;
                        next
                    }
                };
                let entry = get_or_insert_flag(&mut self.flags, spec.id, self.alloc);
                entry.values.push(val);
                Ok(())
            }
            ArgKind::RestUntilFlag => {
                let entry = get_or_insert_flag(&mut self.flags, spec.id, self.alloc);
                if let Some(v) = inline_val {
                    entry.values.push(v);
                } else {
                    let start = self.at;
                    while self.at < self.argv.len() && !self.argv[self.at].starts_with("--") {
                        entry.values.push(self.argv[self.at]);
                        self.at += 1;
                    }
                    entry.span = Some((start, self.at));
                }
                Ok(())
            }
        }
    }
}

fn get_or_insert_flag<'arg, 'alloc, 'v>(
    flags: &'v mut AVec<'alloc, ParsedFlag<'arg, 'alloc>>,
    id: &'static str,
    alloc: Alloc<'alloc>,
) -> &'v mut ParsedFlag<'arg, 'alloc> {
    if let Some(idx) = flags.iter().position(|f| f.id == id) {
        return &mut flags[idx];
    }
    flags.push(ParsedFlag {
        id,
        values: Vec::new_in(alloc),
        span: None,
    });
    let last = flags.len() - 1;
    &mut flags[last]
}

fn flag_matches(spec: &ArgSpec, long: Option<&str>, short: Option<char>) -> bool {
    (long.is_some() && spec.long == long) || (short.is_some() && spec.short == short)
}

fn find_flag_owner(
    app: &AppSpec,
    exclude_cmd: &str,
    long: Option<&str>,
    short: Option<char>,
) -> Option<&'static str> {
    for cmd in app.commands {
        if cmd.hidden || cmd.name == exclude_cmd {
            continue;
        }
        if cmd
            .flags
            .iter()
            .any(|f| !f.hidden && flag_matches(f, long, short))
        {
            return Some(cmd.name);
        }
    }
    for cmd in app.commands {
        if cmd.hidden || cmd.name == exclude_cmd {
            continue;
        }
        if cmd.flags.iter().any(|f| flag_matches(f, long, short)) {
            return Some(cmd.name);
        }
    }
    None
}

/// Render human-readable help for `app` (or a specific `subcommand`) directly to `out`
/// with zero heap allocations.
pub fn render_help(
    app: &AppSpec,
    subcommand: Option<&str>,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    if let Some(cmd) = subcommand.and_then(|sub| find_command(app, sub)) {
        write!(out, "{} {}", app.name, cmd.name)?;
        if !cmd.usage_args.is_empty() {
            write!(out, " {}", cmd.usage_args)?;
        }
        write!(out, "\n\n{}", cmd.about)?;
        if cmd.flags.iter().any(|f| !f.hidden) {
            out.write_str("\n\nOPTIONS")?;
            render_flags_section(cmd.flags, out)?;
        }
        return Ok(());
    }

    write!(
        out,
        "{} — {}\n\nUSAGE\n  {} <command> [options] [dir]\n\nCOMMANDS",
        app.name, app.about, app.name
    )?;

    let mut max_cmd_len = 0usize;
    for cmd in app.commands {
        if !cmd.hidden && cmd.name.len() > max_cmd_len {
            max_cmd_len = cmd.name.len();
        }
    }
    let cmd_col = max_cmd_len.saturating_add(2);
    for cmd in app.commands {
        if cmd.hidden {
            continue;
        }
        write!(out, "\n  {}", cmd.name)?;
        write_spaces(out, cmd_col.saturating_sub(cmd.name.len()))?;
        out.write_str(cmd.about)?;
    }

    if app.common_flags.iter().any(|f| !f.hidden) {
        out.write_str("\n\nCOMMON OPTIONS")?;
        render_flags_section(app.common_flags, out)?;
    }

    if !app.examples.is_empty() {
        out.write_str("\n\nEXAMPLES")?;
        for &ex in app.examples {
            write!(out, "\n  {ex}")?;
        }
    }
    Ok(())
}

/// Render a concise single-line usage summary for `app` listing all visible commands.
pub fn render_usage(app: &AppSpec, out: &mut impl fmt::Write) -> fmt::Result {
    write!(out, "usage: {} <command> [options] (commands: ", app.name)?;
    let mut first = true;
    for cmd in app.commands {
        if cmd.hidden {
            continue;
        }
        if !first {
            out.write_str(", ")?;
        }
        out.write_str(cmd.name)?;
        first = false;
    }
    out.write_char(')')
}

/// Convert `app` (top-level when `subcommand` is `None`, or a specific subcommand when `Some`)
/// into a [`CliSpec`] for `blaze-shell` completions.
pub fn to_cli_spec<'a>(app: &AppSpec, subcommand: Option<&str>, alloc: Alloc<'a>) -> CliSpec<'a> {
    if let Some(sub) = subcommand {
        let mut command = String::from_str_in(app.name, alloc);
        command.push(' ');
        if let Some(cmd) = find_command(app, sub) {
            command.push_str(cmd.name);
            let mut flags = Vec::new_in(alloc);
            for arg in cmd.flags {
                if !arg.hidden {
                    flags.push(arg_to_flag_spec(arg, alloc));
                }
            }
            return CliSpec {
                command,
                flags,
                subcommands: Vec::new_in(alloc),
                source: SpecSource::Manual,
            };
        }
        command.push_str(sub);
        return CliSpec {
            command,
            flags: Vec::new_in(alloc),
            subcommands: Vec::new_in(alloc),
            source: SpecSource::Manual,
        };
    }

    let mut flags = Vec::new_in(alloc);
    for arg in app.common_flags {
        if !arg.hidden {
            flags.push(arg_to_flag_spec(arg, alloc));
        }
    }
    let mut subcommands = Vec::new_in(alloc);
    for cmd in app.commands {
        if !cmd.hidden {
            subcommands.push(SubcommandSpec {
                name: String::from_str_in(cmd.name, alloc),
                description: String::from_str_in(cmd.about, alloc),
            });
        }
    }
    CliSpec {
        command: String::from_str_in(app.name, alloc),
        flags,
        subcommands,
        source: SpecSource::Manual,
    }
}

fn arg_to_flag_spec<'a>(arg: &ArgSpec, alloc: Alloc<'a>) -> FlagSpec<'a> {
    let long = arg.long.map(|name| {
        let mut s = String::new_in(alloc);
        s.push_str("--");
        s.push_str(name);
        s
    });
    let short = arg.short.map(|ch| {
        let mut s = String::new_in(alloc);
        s.push('-');
        s.push(ch);
        s
    });
    let value_hint = arg
        .value_hint
        .and_then(|hint| (!hint.is_empty()).then(|| String::from_str_in(hint, alloc)));
    FlagSpec {
        long,
        short,
        value_hint,
        description: String::from_str_in(arg.about, alloc),
    }
}

fn render_flags_section(flags: &[ArgSpec], out: &mut impl fmt::Write) -> fmt::Result {
    let any_short = flags.iter().any(|f| !f.hidden && f.short.is_some());
    let mut max_width = 0usize;
    for arg in flags {
        if arg.hidden {
            continue;
        }
        let w = flag_label_width(arg, any_short);
        if w > max_width {
            max_width = w;
        }
    }
    let col = max_width.saturating_add(3);
    for arg in flags {
        if arg.hidden {
            continue;
        }
        out.write_char('\n')?;
        let written = write_flag_label(arg, any_short, out)?;
        write_spaces(out, col.saturating_sub(written))?;
        out.write_str(arg.about)?;
    }
    Ok(())
}

fn flag_label_width(arg: &ArgSpec, any_short: bool) -> usize {
    let base = match (arg.short, arg.long) {
        (Some(_), Some(long)) => 8usize.saturating_add(long.len()),
        (None, Some(long)) => {
            let lead = if any_short { 8usize } else { 4usize };
            lead.saturating_add(long.len())
        }
        (Some(_), None) => 4usize,
        (None, None) => 2usize,
    };
    match arg.value_hint {
        Some(hint) if !hint.is_empty() => base.saturating_add(1).saturating_add(hint.len()),
        _ => base,
    }
}

fn write_flag_label(arg: &ArgSpec, any_short: bool, out: &mut impl fmt::Write) -> Result<usize, fmt::Error> {
    let mut width = match (arg.short, arg.long) {
        (Some(short), Some(long)) => {
            write!(out, "  -{short}, --{long}")?;
            8usize.saturating_add(long.len())
        }
        (None, Some(long)) => {
            if any_short {
                write!(out, "      --{long}")?;
                8usize.saturating_add(long.len())
            } else {
                write!(out, "  --{long}")?;
                4usize.saturating_add(long.len())
            }
        }
        (Some(short), None) => {
            write!(out, "  -{short}")?;
            4usize
        }
        (None, None) => {
            out.write_str("  ")?;
            2usize
        }
    };
    if let Some(hint) = arg.value_hint
        && !hint.is_empty()
    {
        write!(out, " {hint}")?;
        width = width.saturating_add(1).saturating_add(hint.len());
    }
    Ok(width)
}

fn write_spaces(out: &mut impl fmt::Write, count: usize) -> fmt::Result {
    for _ in 0..count {
        out.write_char(' ')?;
    }
    Ok(())
}

/// Compile-time reflection bridge for Blaze-edition CLI argument structs (Rule 56: reflection-first).
///
/// Under native Blaze (`cfg(blaze)`), structs project directly into `&'static [ArgSpec]` via
/// `inline for` over `core::reflect::fields_of::<Self>()` with zero proc macros and zero runtime
/// schema allocation. Gated under `#[cfg(blaze)]` so `rustc` compiles cleanly without extensions.
#[allow(unexpected_cfgs)]
#[cfg(blaze)]
pub mod reflect {
    use super::{ArgKind, ArgSpec};

    /// Derive a static `&'static [ArgSpec]` from a Blaze-edition struct at compile time without
    /// proc macros.
    ///
    /// Example with Blaze `inline for` field reflection:
    /// ```ignore
    /// impl ReflectArgs for BuildArgs {
    ///     const SPECS: &'static [ArgSpec] = &const {
    ///         let mut specs = [ArgSpec::EMPTY; core::reflect::field_count::<Self>()];
    ///         let mut i = 0usize;
    ///         inline for field in core::reflect::fields_of::<Self>() {
    ///             specs[i] = ArgSpec {
    ///                 id: field.name,
    ///                 long: Some(field.kebab_name),
    ///                 short: field.short_attr,
    ///                 value_hint: field.value_hint,
    ///                 kind: <field.Ty as ReflectArgKind>::KIND,
    ///                 about: field.doc,
    ///                 hidden: field.hidden,
    ///             };
    ///             i += 1;
    ///         }
    ///         specs
    ///     };
    /// }
    /// ```
    pub trait ReflectArgs: Sized {
        /// Compile-time reflected CLI flag specifications for `Self`.
        const SPECS: &'static [ArgSpec];
    }

    /// Maps a reflected field type to its [`ArgKind`] at compile time.
    pub trait ReflectArgKind {
        const KIND: ArgKind;
    }

    impl ReflectArgKind for bool {
        const KIND: ArgKind = ArgKind::Bool;
    }

    impl<T> ReflectArgKind for Option<T> {
        const KIND: ArgKind = ArgKind::Value;
    }
}

#[allow(unexpected_cfgs)]
#[cfg(blaze)]
pub use reflect::ReflectArgs;

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;

    use blaze_own::{AString, Erased, Inline};

    use super::*;
    use crate::blaze_app::BLAZE_APP;
    use crate::manual::manual_spec_in;

    #[test]
    fn bare_help_and_version_outcomes() {
        let alloc = blaze_own::testing::heap().alloc();
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze"], alloc), Ok(ParseOutcome::Help(None)));
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze", "-h"], alloc), Ok(ParseOutcome::Help(None)));
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze", "--help"], alloc), Ok(ParseOutcome::Help(None)));
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze", "help"], alloc), Ok(ParseOutcome::Help(None)));
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "help", "build"], alloc),
            Ok(ParseOutcome::Help(Some("build")))
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "help", "engine-check"], alloc),
            Ok(ParseOutcome::Help(Some("check")))
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "help", "unknown"], alloc),
            Ok(ParseOutcome::Help(None))
        );
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze", "-V"], alloc), Ok(ParseOutcome::Version));
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze", "--version"], alloc), Ok(ParseOutcome::Version));
        assert_eq!(parse_argv(&BLAZE_APP, &["blaze", "version"], alloc), Ok(ParseOutcome::Version));
    }

    #[test]
    fn subcommand_help_respects_passthrough_delimiter() {
        let alloc = blaze_own::testing::heap().alloc();
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "check", "--help"], alloc),
            Ok(ParseOutcome::Help(Some("check")))
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "engine-check", "-h"], alloc),
            Ok(ParseOutcome::Help(Some("check")))
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "warm-toolchain", "--help"], alloc),
            Ok(ParseOutcome::Help(Some("warm-toolchain")))
        );

        let Ok(ParseOutcome::Command(parsed)) =
            parse_argv(&BLAZE_APP, &["blaze", "run", "--open", "--", "--help", "--open"], alloc)
        else {
            panic!("expected Command for run with passthrough --help");
        };
        assert_eq!(parsed.command, "run");
        assert!(parsed.has("open"));
        assert_eq!(parsed.passthrough.as_slice(), &["--help", "--open"]);
    }

    #[test]
    fn renamed_and_unknown_commands_and_wrong_command_flags() {
        let alloc = blaze_own::testing::heap().alloc();
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "export"], alloc),
            Err(ParseError::Renamed {
                old: "export",
                instead: "use `blaze build` to build a standalone native executable",
            })
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "frobnicate"], alloc),
            Err(ParseError::UnknownCommand { got: "frobnicate" })
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "check", "--open"], alloc),
            Err(ParseError::WrongCommandFlag {
                flag: "--open",
                command: "check",
                valid_for: "run",
            })
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "check", "-o", "./app"], alloc),
            Err(ParseError::WrongCommandFlag {
                flag: "-o",
                command: "check",
                valid_for: "build",
            })
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "check", "--nonexistent"], alloc),
            Err(ParseError::UnknownFlag {
                command: "check",
                flag: "--nonexistent",
            })
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "check", "--", "arg"], alloc),
            Err(ParseError::PassthroughNotAllowed { command: "check" })
        );
        assert_eq!(
            parse_argv(&BLAZE_APP, &["blaze", "build", "-o"], alloc),
            Err(ParseError::MissingValue { flag: "-o" })
        );
    }

    #[test]
    fn parses_values_multivalues_short_equals_and_rest_until_flag() {
        let alloc = blaze_own::testing::heap().alloc();
        let argv = [
            "blaze",
            "check",
            "crates/demo",
            "--color=always",
            "--view",
            "json",
            "--fix",
            "E001",
            "src/lib.rs",
            "--stats",
            "--workers",
            "8",
            "--driver=ready",
            "--names-oracle",
            "--block-scopes=bodies",
        ];
        let Ok(ParseOutcome::Command(parsed)) = parse_argv(&BLAZE_APP, &argv, alloc) else {
            panic!("expected parsed check command");
        };
        assert_eq!(parsed.command, "check");
        assert_eq!(parsed.positionals.as_slice(), &["crates/demo"]);
        assert_eq!(parsed.last_value("color"), Some("always"));
        assert_eq!(parsed.last_value("view"), Some("json"));
        assert_eq!(parsed.span("fix"), Some((7, 9)));
        let mut fix_vals = Vec::new_in(alloc);
        for v in parsed.values("fix") {
            fix_vals.push(v);
        }
        assert_eq!(fix_vals.as_slice(), &["E001", "src/lib.rs"]);
        assert!(parsed.has("stats"));
        assert_eq!(parsed.last_value("workers"), Some("8"));
        assert_eq!(parsed.last_value("driver"), Some("ready"));
        assert!(parsed.has("names-oracle"));
        assert_eq!(parsed.last_value("block-scopes"), Some("bodies"));

        let build_argv = [
            "blaze",
            "build",
            "-p",
            "alpha",
            "-p=beta",
            "--package",
            "gamma",
            "--package=delta",
            "--bin=cli",
            "-o=./out/cli",
        ];
        let Ok(ParseOutcome::Command(built)) = parse_argv(&BLAZE_APP, &build_argv, alloc) else {
            panic!("expected parsed build command");
        };
        let mut pkgs = Vec::new_in(alloc);
        for p in built.values("package") {
            pkgs.push(p);
        }
        assert_eq!(pkgs.as_slice(), &["alpha", "beta", "gamma", "delta"]);
        assert_eq!(built.last_value("package"), Some("delta"));
        assert_eq!(built.last_value("bin"), Some("cli"));
        assert_eq!(built.last_value("output"), Some("./out/cli"));
    }

    #[test]
    fn separated_arg_and_alloc_lifetimes_work_with_inline_scratch() {
        let argv = ["blaze", "build", "-p", "hello", "-o", "./hello"];
        let (pkg, out_path): (Option<&str>, Option<&str>) = {
            let scratch = Inline::<4096>::new();
            let erased = Erased::new(&scratch);
            let Ok(ParseOutcome::Command(parsed)) = parse_argv(&BLAZE_APP, &argv, erased.handle()) else {
                panic!("expected Command");
            };
            (parsed.last_value("package"), parsed.last_value("output"))
        };
        assert_eq!(pkg, Some("hello"));
        assert_eq!(out_path, Some("./hello"));
    }

    #[test]
    fn render_help_usage_and_manual_spec_stay_in_sync() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut top = String::new_in(alloc);
        render_help(&BLAZE_APP, None, &mut top).expect("render top help");
        assert!(top.as_str().contains("USAGE"));
        assert!(top.as_str().contains("build"));
        assert!(top.as_str().contains("check"));
        assert!(!top.as_str().contains("engine-dry"));
        assert!(!top.as_str().contains("journal-helper"));

        let mut build_help = String::new_in(alloc);
        render_help(&BLAZE_APP, Some("build"), &mut build_help).expect("render build help");
        assert!(build_help.as_str().contains("blaze build [dir] [options]"));
        assert!(build_help.as_str().contains("-o, --output <path>"));
        assert!(!build_help.as_str().contains("--names-oracle"));

        let mut usage = String::new_in(alloc);
        render_usage(&BLAZE_APP, &mut usage).expect("render usage");
        assert!(usage.as_str().starts_with("usage: blaze <command>"));
        assert!(usage.as_str().contains("build"));
        assert!(!usage.as_str().contains("engine-dry"));

        let top_spec = manual_spec_in("blaze", None, alloc).expect("blaze top manual spec");
        assert!(top_spec.subcommands.iter().any(|s| s.name.as_str() == "build"));
        assert!(!top_spec.subcommands.iter().any(|s| s.name.as_str() == "export"));
        assert!(!top_spec.subcommands.iter().any(|s| s.name.as_str() == "engine-dry"));
        assert!(top_spec.flags.iter().any(|f| f.long.as_ref().map(AString::as_str) == Some("--output")));

        let build_spec = manual_spec_in("blaze", Some("build"), alloc).expect("blaze build manual spec");
        assert_eq!(build_spec.command.as_str(), "blaze build");
        assert!(build_spec.flags.iter().any(|f| {
            f.long.as_ref().map(AString::as_str) == Some("--output")
                && f.short.as_ref().map(AString::as_str) == Some("-o")
        }));
        assert!(manual_spec_in("blaze", Some("not-a-cmd"), alloc).is_none());

        let mut err_buf = String::new_in(alloc);
        let err = ParseError::WrongCommandFlag {
            flag: "--open",
            command: "check",
            valid_for: "run",
        };
        write!(err_buf, "{err}").expect("display error");
        assert!(err_buf.as_str().contains("--open"));
        assert!(err_buf.as_str().contains("run"));
    }

    #[test]
    fn parses_all_subcommands_bare_fix_and_hidden_commands() {
        let alloc = blaze_own::testing::heap().alloc();

        let Ok(ParseOutcome::Command(bare_fix)) =
            parse_argv(&BLAZE_APP, &["blaze", "check", "--fix"], alloc)
        else {
            panic!("expected Command for bare --fix");
        };
        assert!(bare_fix.has("fix"));
        assert_eq!(bare_fix.span("fix"), Some((3, 3)));
        assert_eq!(bare_fix.values("fix").next(), None);

        let Ok(ParseOutcome::Command(tested)) =
            parse_argv(&BLAZE_APP, &["blaze", "test", "-p", "a", "-p", "b", "--", "--nocapture"], alloc)
        else {
            panic!("expected Command for test");
        };
        assert_eq!(tested.command, "test");
        assert_eq!(tested.passthrough.as_slice(), &["--nocapture"]);
        let mut pkgs = Vec::new_in(alloc);
        for p in tested.values("package") {
            pkgs.push(p);
        }
        assert_eq!(pkgs.as_slice(), &["a", "b"]);

        let Ok(ParseOutcome::Command(ci)) =
            parse_argv(&BLAZE_APP, &["blaze", "ci", "--format=github", "--todo", "out.json", "--no-test"], alloc)
        else {
            panic!("expected Command for ci");
        };
        assert_eq!(ci.last_value("format"), Some("github"));
        assert_eq!(ci.last_value("todo"), Some("out.json"));
        assert!(ci.has("no-test"));

        let Ok(ParseOutcome::Command(insp)) =
            parse_argv(&BLAZE_APP, &["blaze", "inspect", "src/lib.rs::f", "--json", "--all-exprs", "--edition=2024"], alloc)
        else {
            panic!("expected Command for inspect");
        };
        assert_eq!(insp.positionals.as_slice(), &["src/lib.rs::f"]);
        assert!(insp.has("json"));
        assert!(insp.has("all-exprs"));
        assert_eq!(insp.last_value("edition"), Some("2024"));

        let Ok(ParseOutcome::Command(cleaned)) =
            parse_argv(&BLAZE_APP, &["blaze", "clean", "--store"], alloc)
        else {
            panic!("expected Command for clean");
        };
        assert!(cleaned.has("store"));

        let Ok(ParseOutcome::Command(gc)) =
            parse_argv(&BLAZE_APP, &["blaze", "gc", "--background", "--max-gb=16"], alloc)
        else {
            panic!("expected Command for gc");
        };
        assert!(gc.has("background"));
        assert_eq!(gc.last_value("max-gb"), Some("16"));

        let Ok(ParseOutcome::Command(dry)) =
            parse_argv(&BLAZE_APP, &["blaze", "engine-dry", "src/lib.rs", "--driver=ready"], alloc)
        else {
            panic!("expected Command for engine-dry");
        };
        assert_eq!(dry.command, "engine-dry");
        assert_eq!(dry.positionals.as_slice(), &["src/lib.rs"]);
        assert_eq!(dry.last_value("driver"), Some("ready"));
    }
}
