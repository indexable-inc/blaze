//! Declarative, pure `#![no_std]` CLI argument parser and ANSI-styled `--help` formatter.
//!
//! Sans-IO by construction: every entry point operates on an explicit `&[&str]` slice and an
//! explicit allocator with zero ambient `env::args` access.
//!
//! - Const-constructible declarative specs ([`CommandSpec`], [`FlagSpec`], [`OptSpec`],
//!   [`PositionalSpec`], [`SubcommandSpec`]) require zero heap allocation to define.
//! - POSIX and GNU parsing conventions: `--flag`, `--opt=val`, `--opt val`, `-f`, `-fff`
//!   occurrence counting, `-abc` combined short flags, `-oVAL` and `-o VAL`, `--` option
//!   terminator, iterative arbitrarily nested subcommand dispatch, built-in `--help` / `-h` and
//!   `--version` / `-V`.
//! - Typo suggestions for unknown `--long` flags/options and unknown subcommands powered by
//!   [`crate::diff::edit_distance`].
//! - Zero-allocation `--help` renderer ([`format_help_into`], [`format_help_in`]) with optional
//!   ANSI styling via [`crate::ansi::Style`].

use core::alloc::Allocator;
use core::fmt::{self, Write as _};
use core::str::FromStr;

use blaze_own::{AllocFailure, String, Vec};

use crate::ansi::{Color, Style};
use crate::diff::{DiffError, edit_distance};

/// Maximum Myers edit distance for typo suggestions.
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// Search step budget passed to [`crate::diff::edit_distance`] for typo matching.
const SUGGESTION_DIFF_BUDGET: usize = 64;

/// Specification of a boolean/counting flag (for example `-v` / `--verbose`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlagSpec<'a> {
    /// Canonical long name without leading `--`.
    pub long: &'a str,
    /// Optional single-character short alias (for example `Some('v')`).
    pub short: Option<char>,
    /// One-line description shown in `--help`.
    pub help: &'a str,
}

/// Specification of a value-taking option (for example `-o <FILE>` / `--output <FILE>`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OptSpec<'a> {
    /// Canonical long name without leading `--`.
    pub long: &'a str,
    /// Optional single-character short alias.
    pub short: Option<char>,
    /// Placeholder value name shown in `--help` (for example `"FILE"`).
    pub value_name: &'a str,
    /// Whether the option must be supplied when `default` is `None`.
    pub required: bool,
    /// Whether the option may be repeated (`--opt a --opt b`).
    pub multiple: bool,
    /// Fallback value returned by [`Matches::opt`] when not passed on the command line.
    pub default: Option<&'a str>,
    /// One-line description shown in `--help`.
    pub help: &'a str,
}

/// Specification of a positional argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PositionalSpec<'a> {
    /// Display name shown in `Usage:` and `Arguments:`.
    pub name: &'a str,
    /// Whether this positional argument is required.
    pub required: bool,
    /// Whether this positional accepts arbitrarily many trailing values.
    pub multiple: bool,
    /// One-line description shown in `--help`.
    pub help: &'a str,
}

/// Specification of a nested subcommand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SubcommandSpec<'a> {
    /// Token matched on the command line to dispatch into `spec`.
    pub name: &'a str,
    /// Short description shown in the parent command's `Commands:` table.
    pub about: &'a str,
    /// Command specification active once this subcommand is selected.
    pub spec: &'a CommandSpec<'a>,
}

/// Declarative specification of a command or subcommand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommandSpec<'a> {
    /// Command name (shown in `Usage:` and recorded in [`Matches::command_path`]).
    pub name: &'a str,
    /// Short summary shown at the top of `--help`.
    pub about: &'a str,
    /// Optional version string enabling `--version` / `-V`.
    pub version: Option<&'a str>,
    /// Boolean and countable flags accepted by this command.
    pub flags: &'a [FlagSpec<'a>],
    /// Value-taking options accepted by this command.
    pub opts: &'a [OptSpec<'a>],
    /// Positional arguments accepted by this command.
    pub positionals: &'a [PositionalSpec<'a>],
    /// Nested subcommands dispatched from this command.
    pub subcommands: &'a [SubcommandSpec<'a>],
}

impl<'a> CommandSpec<'a> {
    /// Parse `args` (arguments after `argv[0]`) against this command specification.
    pub fn parse_in<A: Allocator + Copy>(
        &'a self,
        args: &[&'a str],
        alloc: A,
    ) -> Result<ParseOutcome<'a, A>, CliError<'a>> {
        parse_in(self, args, alloc)
    }
}

/// Typed failure returned while parsing arguments or converting an option value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CliError<'a> {
    /// An unrecognized `--<flag>` option was supplied.
    UnknownFlag { flag: &'a str, suggestion: Option<&'a str> },
    /// An unrecognized `-<c>` short flag was supplied.
    UnknownShort { flag: char },
    /// An unrecognized subcommand was supplied.
    UnknownSubcommand { name: &'a str, suggestion: Option<&'a str> },
    /// A value-taking option was not followed by a value.
    MissingValue { opt: &'a str },
    /// A required option with no default was not supplied.
    MissingRequiredOpt { opt: &'a str },
    /// A required positional argument was not supplied.
    MissingRequiredPositional { name: &'a str },
    /// A single-value option (`multiple == false`) was passed more than once.
    DuplicateOpt { opt: &'a str },
    /// A boolean/counting flag was given an inline `=value`.
    UnexpectedValue { flag: &'a str },
    /// More positional arguments were passed than the specification accepts.
    UnexpectedPositional { value: &'a str },
    /// [`Matches::opt_parse`] failed to parse an option's string value into `T`.
    InvalidValue { opt: &'a str, value: &'a str },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl fmt::Display for CliError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CliError::UnknownFlag { flag, suggestion: Some(suggest) } => {
                write!(f, "bstd::cli: unknown option `--{flag}`; did you mean `--{suggest}`?")
            }
            CliError::UnknownFlag { flag, suggestion: None } => {
                write!(f, "bstd::cli: unknown option `--{flag}`")
            }
            CliError::UnknownShort { flag } => {
                write!(f, "bstd::cli: unknown short flag `-{flag}`")
            }
            CliError::UnknownSubcommand { name, suggestion: Some(suggest) } => {
                write!(f, "bstd::cli: unknown subcommand `{name}`; did you mean `{suggest}`?")
            }
            CliError::UnknownSubcommand { name, suggestion: None } => {
                write!(f, "bstd::cli: unknown subcommand `{name}`")
            }
            CliError::MissingValue { opt } => {
                write!(f, "bstd::cli: option `--{opt}` requires a value")
            }
            CliError::MissingRequiredOpt { opt } => {
                write!(f, "bstd::cli: missing required option `--{opt}`")
            }
            CliError::MissingRequiredPositional { name } => {
                write!(f, "bstd::cli: missing required argument `<{name}>`")
            }
            CliError::DuplicateOpt { opt } => {
                write!(f, "bstd::cli: option `--{opt}` may only be passed once")
            }
            CliError::UnexpectedValue { flag } => {
                write!(f, "bstd::cli: flag `--{flag}` does not take a value")
            }
            CliError::UnexpectedPositional { value } => {
                write!(f, "bstd::cli: unexpected argument `{value}`")
            }
            CliError::InvalidValue { opt, value } => {
                write!(f, "bstd::cli: invalid value `{value}` for option `--{opt}`")
            }
            CliError::Alloc(err) => write!(f, "bstd::cli: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for CliError<'_> {}

/// Outcome of [`parse_in`].
pub enum ParseOutcome<'a, A: Allocator> {
    /// Arguments parsed cleanly into [`Matches`].
    Matched(Matches<'a, A>),
    /// The caller asked for `--help` / `-h` on `command`.
    Help { command: &'a CommandSpec<'a> },
    /// The caller asked for `--version` / `-V`.
    Version { name: &'a str, version: &'a str },
}

impl<A: Allocator> fmt::Debug for ParseOutcome<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseOutcome::Matched(m) => f.debug_tuple("Matched").field(m).finish(),
            ParseOutcome::Help { command } => {
                f.debug_struct("Help").field("command", command).finish()
            }
            ParseOutcome::Version { name, version } => f
                .debug_struct("Version")
                .field("name", name)
                .field("version", version)
                .finish(),
        }
    }
}

impl<A: Allocator> PartialEq for ParseOutcome<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ParseOutcome::Matched(a), ParseOutcome::Matched(b)) => a == b,
            (ParseOutcome::Help { command: a }, ParseOutcome::Help { command: b }) => a == b,
            (
                ParseOutcome::Version { name: na, version: va },
                ParseOutcome::Version { name: nb, version: vb },
            ) => na == nb && va == vb,
            (
                ParseOutcome::Matched(_),
                ParseOutcome::Help { .. } | ParseOutcome::Version { .. },
            )
            | (
                ParseOutcome::Help { .. },
                ParseOutcome::Matched(_) | ParseOutcome::Version { .. },
            )
            | (
                ParseOutcome::Version { .. },
                ParseOutcome::Matched(_) | ParseOutcome::Help { .. },
            ) => false,
        }
    }
}

impl<A: Allocator> Eq for ParseOutcome<'_, A> {}

/// Parsed flags, options, positionals, and subcommand path returned by [`parse_in`].
pub struct Matches<'a, A: Allocator> {
    command_path: Vec<&'a str, A>,
    flags: Vec<(&'a str, usize), A>,
    opts: Vec<(&'a str, &'a str), A>,
    positionals: Vec<&'a str, A>,
    active_spec: &'a CommandSpec<'a>,
}

impl<A: Allocator> fmt::Debug for Matches<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Matches")
            .field("command_path", &self.command_path.as_slice())
            .field("flags", &self.flags.as_slice())
            .field("opts", &self.opts.as_slice())
            .field("positionals", &self.positionals.as_slice())
            .field("active_spec", &self.active_spec.name)
            .finish()
    }
}

impl<A: Allocator> PartialEq for Matches<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        self.command_path.as_slice() == other.command_path.as_slice()
            && self.flags.as_slice() == other.flags.as_slice()
            && self.opts.as_slice() == other.opts.as_slice()
            && self.positionals.as_slice() == other.positionals.as_slice()
            && self.active_spec == other.active_spec
    }
}

impl<A: Allocator> Eq for Matches<'_, A> {}

impl<'a, A: Allocator> Matches<'a, A> {
    /// Full resolved command path starting from the root command (for example `["blaze", "cache", "gc"]`).
    #[inline]
    #[must_use]
    pub fn command_path(&self) -> &[&'a str] {
        self.command_path.as_slice()
    }

    /// Return `true` if flag `long` was passed at least once.
    #[inline]
    #[must_use]
    pub fn flag(&self, long: &str) -> bool {
        self.flag_count(long) > 0
    }

    /// Number of times flag `long` was passed (supports repeated flags like `-vvv`).
    #[must_use]
    pub fn flag_count(&self, long: &str) -> usize {
        for &(name, count) in self.flags.as_slice() {
            if name == long {
                return count;
            }
        }
        0
    }

    /// Last matched value for option `long`, or its specification's `default` when not passed.
    #[must_use]
    pub fn opt(&self, long: &str) -> Option<&'a str> {
        self.opt_with_key(long).map(|(_, val)| val)
    }

    /// Collect all matched values for option `long` in encounter order (or `[default]` when none
    /// were supplied and the active command defines a default).
    pub fn opt_all<B: Allocator>(
        &self,
        long: &str,
        alloc: B,
    ) -> Result<Vec<&'a str, B>, AllocFailure> {
        let mut out = Vec::new_in(alloc);
        for &(name, val) in self.opts.as_slice() {
            if name == long {
                out.try_push(val).map_err(|r| r.why)?;
            }
        }
        if out.is_empty()
            && let Some(default) = self.default_for_opt(long)
        {
            out.try_push(default).map_err(|r| r.why)?;
        }
        Ok(out)
    }

    /// Parse the last matched (or defaulted) value for option `long` using [`FromStr`].
    pub fn opt_parse<T: FromStr>(&self, long: &str) -> Result<Option<T>, CliError<'a>> {
        let Some((key, value)) = self.opt_with_key(long) else {
            return Ok(None);
        };
        match value.parse::<T>() {
            Ok(parsed) => Ok(Some(parsed)),
            Err(_) => Err(CliError::InvalidValue { opt: key, value }),
        }
    }

    /// Return the 0-based positional argument at `index`, if present.
    #[inline]
    #[must_use]
    pub fn positional(&self, index: usize) -> Option<&'a str> {
        self.positionals.as_slice().get(index).copied()
    }

    /// All positional arguments in encounter order.
    #[inline]
    #[must_use]
    pub fn positionals(&self) -> &[&'a str] {
        self.positionals.as_slice()
    }

    /// Active leaf subcommand name, or `None` when the root command itself ran.
    #[inline]
    #[must_use]
    pub fn subcommand(&self) -> Option<&'a str> {
        if self.command_path.len() > 1 {
            self.command_path.as_slice().last().copied()
        } else {
            None
        }
    }

    /// Active [`CommandSpec`] after subcommand dispatch.
    #[inline]
    #[must_use]
    pub fn active_command(&self) -> &'a CommandSpec<'a> {
        self.active_spec
    }

    fn opt_with_key(&self, long: &str) -> Option<(&'a str, &'a str)> {
        for &(name, val) in self.opts.as_slice().iter().rev() {
            if name == long {
                return Some((name, val));
            }
        }
        for spec in self.active_spec.opts {
            if spec.long == long {
                return spec.default.map(|d| (spec.long, d));
            }
        }
        None
    }

    fn default_for_opt(&self, long: &str) -> Option<&'a str> {
        for spec in self.active_spec.opts {
            if spec.long == long {
                return spec.default;
            }
        }
        None
    }
}

/// Parse `args` (arguments after `argv[0]`) against `spec`, allocating results in `alloc`.
pub fn parse_in<'a, A: Allocator + Copy>(
    spec: &'a CommandSpec<'a>,
    args: &[&'a str],
    alloc: A,
) -> Result<ParseOutcome<'a, A>, CliError<'a>> {
    parse_in_planted(spec, args, alloc, false)
}

fn parse_in_planted<'a, A: Allocator + Copy>(
    spec: &'a CommandSpec<'a>,
    args: &[&'a str],
    alloc: A,
    plant_ignore_double_dash: bool,
) -> Result<ParseOutcome<'a, A>, CliError<'a>> {
    let mut active = spec;
    let mut command_path = Vec::new_in(alloc);
    command_path.try_push(spec.name).map_err(|r| CliError::Alloc(r.why))?;
    let mut flags: Vec<(&'a str, usize), A> = Vec::new_in(alloc);
    let mut opts: Vec<(&'a str, &'a str), A> = Vec::new_in(alloc);
    let mut positionals: Vec<&'a str, A> = Vec::new_in(alloc);

    let mut after_double_dash = false;
    let mut idx = 0usize;

    while let Some(&arg) = args.get(idx) {
        if after_double_dash {
            positionals.try_push(arg).map_err(|r| CliError::Alloc(r.why))?;
            idx += 1;
            continue;
        }

        if arg == "--" {
            if !plant_ignore_double_dash {
                after_double_dash = true;
            }
            idx += 1;
            continue;
        }

        if arg == "--help" && !has_long(active, "help") {
            return Ok(ParseOutcome::Help { command: active });
        }

        if arg == "--version"
            && !has_long(active, "version")
            && let Some(version) = active.version
        {
            return Ok(ParseOutcome::Version { name: active.name, version });
        }

        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline_val) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (rest, None),
            };
            if let Some(flag_spec) = find_long_flag(active, name) {
                if inline_val.is_some() {
                    return Err(CliError::UnexpectedValue { flag: flag_spec.long });
                }
                bump_flag(&mut flags, flag_spec.long)?;
                idx += 1;
                continue;
            }
            if let Some(opt_spec) = find_long_opt(active, name) {
                let value = match inline_val {
                    Some(v) => v,
                    None => {
                        idx += 1;
                        match args.get(idx) {
                            Some(&v) => v,
                            None => return Err(CliError::MissingValue { opt: opt_spec.long }),
                        }
                    }
                };
                record_opt(&mut opts, opt_spec, value)?;
                idx += 1;
                continue;
            }
            let suggestion = suggest_long_option(active, name, alloc)?;
            return Err(CliError::UnknownFlag { flag: name, suggestion });
        }

        if let Some(rest) = arg.strip_prefix('-')
            && !rest.is_empty()
        {
            for (byte_pos, ch) in rest.char_indices() {
                if ch == 'h' && !has_short(active, 'h') {
                    return Ok(ParseOutcome::Help { command: active });
                }
                if ch == 'V'
                    && !has_short(active, 'V')
                    && let Some(version) = active.version
                {
                    return Ok(ParseOutcome::Version { name: active.name, version });
                }
                if let Some(flag_spec) = find_short_flag(active, ch) {
                    bump_flag(&mut flags, flag_spec.long)?;
                    continue;
                }
                if let Some(opt_spec) = find_short_opt(active, ch) {
                    let val_start = byte_pos + ch.len_utf8();
                    let tail = rest.get(val_start..).unwrap_or("");
                    let value = if !tail.is_empty() {
                        tail
                    } else {
                        idx += 1;
                        match args.get(idx) {
                            Some(&v) => v,
                            None => return Err(CliError::MissingValue { opt: opt_spec.long }),
                        }
                    };
                    record_opt(&mut opts, opt_spec, value)?;
                    break;
                }
                return Err(CliError::UnknownShort { flag: ch });
            }
            idx += 1;
            continue;
        }

        if !active.subcommands.is_empty() && positionals.is_empty() {
            if let Some(sub) = find_subcommand(active, arg) {
                active = sub.spec;
                command_path.try_push(sub.name).map_err(|r| CliError::Alloc(r.why))?;
                idx += 1;
                continue;
            }
            if active.positionals.is_empty() {
                let suggestion = suggest_subcommand(active, arg, alloc)?;
                return Err(CliError::UnknownSubcommand { name: arg, suggestion });
            }
        }

        positionals.try_push(arg).map_err(|r| CliError::Alloc(r.why))?;
        idx += 1;
    }

    validate_positionals(active, positionals.as_slice())?;
    validate_required_opts(active, opts.as_slice())?;

    Ok(ParseOutcome::Matched(Matches {
        command_path,
        flags,
        opts,
        positionals,
        active_spec: active,
    }))
}

fn has_long(spec: &CommandSpec<'_>, long: &str) -> bool {
    find_long_flag(spec, long).is_some() || find_long_opt(spec, long).is_some()
}

fn has_short(spec: &CommandSpec<'_>, ch: char) -> bool {
    find_short_flag(spec, ch).is_some() || find_short_opt(spec, ch).is_some()
}

fn find_long_flag<'a>(spec: &'a CommandSpec<'a>, long: &str) -> Option<&'a FlagSpec<'a>> {
    for f in spec.flags {
        if f.long == long {
            return Some(f);
        }
    }
    None
}

fn find_short_flag<'a>(spec: &'a CommandSpec<'a>, ch: char) -> Option<&'a FlagSpec<'a>> {
    for f in spec.flags {
        if f.short == Some(ch) {
            return Some(f);
        }
    }
    None
}

fn find_long_opt<'a>(spec: &'a CommandSpec<'a>, long: &str) -> Option<&'a OptSpec<'a>> {
    for o in spec.opts {
        if o.long == long {
            return Some(o);
        }
    }
    None
}

fn find_short_opt<'a>(spec: &'a CommandSpec<'a>, ch: char) -> Option<&'a OptSpec<'a>> {
    for o in spec.opts {
        if o.short == Some(ch) {
            return Some(o);
        }
    }
    None
}

fn find_subcommand<'a>(spec: &'a CommandSpec<'a>, name: &str) -> Option<&'a SubcommandSpec<'a>> {
    for sub in spec.subcommands {
        if sub.name == name {
            return Some(sub);
        }
    }
    None
}

fn bump_flag<'a, A: Allocator>(
    flags: &mut Vec<(&'a str, usize), A>,
    long: &'a str,
) -> Result<(), CliError<'a>> {
    for entry in flags.as_mut_slice() {
        if entry.0 == long {
            entry.1 = entry.1.saturating_add(1);
            return Ok(());
        }
    }
    flags.try_push((long, 1)).map_err(|r| CliError::Alloc(r.why))
}

fn record_opt<'a, A: Allocator>(
    opts: &mut Vec<(&'a str, &'a str), A>,
    spec: &'a OptSpec<'a>,
    value: &'a str,
) -> Result<(), CliError<'a>> {
    if !spec.multiple {
        for &(name, _) in opts.as_slice() {
            if name == spec.long {
                return Err(CliError::DuplicateOpt { opt: spec.long });
            }
        }
    }
    opts.try_push((spec.long, value)).map_err(|r| CliError::Alloc(r.why))
}

fn validate_positionals<'a>(
    spec: &'a CommandSpec<'a>,
    positionals: &[&'a str],
) -> Result<(), CliError<'a>> {
    let accepts_multiple = spec.positionals.iter().any(|p| p.multiple);
    if !accepts_multiple
        && let Some(&extra) = positionals.get(spec.positionals.len())
    {
        return Err(CliError::UnexpectedPositional { value: extra });
    }
    for (idx, pos_spec) in spec.positionals.iter().enumerate() {
        if pos_spec.required && positionals.get(idx).is_none() {
            return Err(CliError::MissingRequiredPositional { name: pos_spec.name });
        }
    }
    Ok(())
}

fn validate_required_opts<'a>(
    spec: &'a CommandSpec<'a>,
    opts: &[(&'a str, &'a str)],
) -> Result<(), CliError<'a>> {
    for opt_spec in spec.opts {
        if opt_spec.required && opt_spec.default.is_none() {
            let present = opts.iter().any(|&(name, _)| name == opt_spec.long);
            if !present {
                return Err(CliError::MissingRequiredOpt { opt: opt_spec.long });
            }
        }
    }
    Ok(())
}

fn consider_candidate<'a, A: Allocator + Copy>(
    query: &str,
    candidate: &'a str,
    best: &mut Option<(&'a str, usize)>,
    alloc: A,
) -> Result<(), CliError<'a>> {
    if query.is_empty() || candidate.is_empty() {
        return Ok(());
    }
    if query.len().abs_diff(candidate.len()) > MAX_SUGGESTION_DISTANCE {
        return Ok(());
    }
    let dist = match edit_distance(query.as_bytes(), candidate.as_bytes(), SUGGESTION_DIFF_BUDGET, alloc) {
        Ok(d) => d,
        Err(DiffError::BudgetExceeded { .. }) => return Ok(()),
        Err(DiffError::Alloc(err)) => return Err(CliError::Alloc(err)),
    };
    if dist <= MAX_SUGGESTION_DISTANCE {
        let is_better = match *best {
            None => true,
            Some((_, prev)) => dist < prev,
        };
        if is_better {
            *best = Some((candidate, dist));
        }
    }
    Ok(())
}

fn suggest_long_option<'a, A: Allocator + Copy>(
    spec: &'a CommandSpec<'a>,
    query: &str,
    alloc: A,
) -> Result<Option<&'a str>, CliError<'a>> {
    let mut best: Option<(&'a str, usize)> = None;
    for f in spec.flags {
        consider_candidate(query, f.long, &mut best, alloc)?;
    }
    for o in spec.opts {
        consider_candidate(query, o.long, &mut best, alloc)?;
    }
    Ok(best.map(|(name, _)| name))
}

fn suggest_subcommand<'a, A: Allocator + Copy>(
    spec: &'a CommandSpec<'a>,
    query: &str,
    alloc: A,
) -> Result<Option<&'a str>, CliError<'a>> {
    let mut best: Option<(&'a str, usize)> = None;
    for sub in spec.subcommands {
        consider_candidate(query, sub.name, &mut best, alloc)?;
    }
    Ok(best.map(|(name, _)| name))
}

/// Format a `--help` page for `spec` into `out`, with optional ANSI coloring.
pub fn format_help_into<W: fmt::Write>(
    spec: &CommandSpec<'_>,
    color: bool,
    out: &mut W,
) -> fmt::Result {
    let palette = HelpPalette::new(color);

    if !spec.about.is_empty() {
        writeln!(out, "{}", spec.about)?;
        out.write_char('\n')?;
    }

    // Usage line.
    write!(
        out,
        "{} {} {}",
        palette.heading.paint("Usage:"),
        palette.literal.paint(spec.name),
        palette.placeholder.paint("[OPTIONS]")
    )?;
    for pos in spec.positionals {
        out.write_char(' ')?;
        let (left, right) = if pos.required { ("<", ">") } else { ("[", "]") };
        write!(
            out,
            "{}",
            palette.placeholder.paint(Bracketed { left, mid: pos.name, right })
        )?;
        if pos.multiple {
            write!(out, "{}", palette.placeholder.paint("..."))?;
        }
    }
    if !spec.subcommands.is_empty() {
        write!(out, " {}", palette.placeholder.paint("<COMMAND>"))?;
    }
    out.write_char('\n')?;

    // Subcommands table.
    if !spec.subcommands.is_empty() {
        out.write_char('\n')?;
        writeln!(out, "{}", palette.heading.paint("Commands:"))?;
        let mut max_w = 0usize;
        for sub in spec.subcommands {
            max_w = max_w.max(sub.name.len());
        }
        for sub in spec.subcommands {
            write!(out, "  {}", palette.literal.paint(sub.name))?;
            if !sub.about.is_empty() {
                write_spaces(out, max_w.saturating_sub(sub.name.len()) + 2)?;
                out.write_str(sub.about)?;
            }
            out.write_char('\n')?;
        }
    }

    // Positionals table.
    if !spec.positionals.is_empty() {
        out.write_char('\n')?;
        writeln!(out, "{}", palette.heading.paint("Arguments:"))?;
        let mut max_w = 0usize;
        for pos in spec.positionals {
            max_w = max_w.max(positional_label_len(pos));
        }
        for pos in spec.positionals {
            out.write_str("  ")?;
            let (left, right) = if pos.required { ("<", ">") } else { ("[", "]") };
            write!(
                out,
                "{}",
                palette.placeholder.paint(Bracketed { left, mid: pos.name, right })
            )?;
            if pos.multiple {
                write!(out, "{}", palette.placeholder.paint("..."))?;
            }
            if !pos.help.is_empty() {
                let w = positional_label_len(pos);
                write_spaces(out, max_w.saturating_sub(w) + 2)?;
                out.write_str(pos.help)?;
            }
            out.write_char('\n')?;
        }
    }

    // Options table.
    let show_builtin_help = !has_long(spec, "help");
    let show_builtin_version = spec.version.is_some() && !has_long(spec, "version");
    let help_short = if has_short(spec, 'h') { None } else { Some('h') };
    let version_short = if has_short(spec, 'V') { None } else { Some('V') };

    let mut max_opt_w = 0usize;
    for f in spec.flags {
        max_opt_w = max_opt_w.max(option_col_len(f.long, None));
    }
    for o in spec.opts {
        max_opt_w = max_opt_w.max(option_col_len(o.long, Some(o.value_name)));
    }
    if show_builtin_help {
        max_opt_w = max_opt_w.max(option_col_len("help", None));
    }
    if show_builtin_version {
        max_opt_w = max_opt_w.max(option_col_len("version", None));
    }

    out.write_char('\n')?;
    writeln!(out, "{}", palette.heading.paint("Options:"))?;
    for f in spec.flags {
        write_option_row(
            out,
            OptionRow {
                short: f.short,
                long: f.long,
                value_name: None,
                help: f.help,
                default: None,
                required: false,
            },
            max_opt_w,
            palette,
        )?;
    }
    for o in spec.opts {
        write_option_row(
            out,
            OptionRow {
                short: o.short,
                long: o.long,
                value_name: Some(o.value_name),
                help: o.help,
                default: o.default,
                required: o.required && o.default.is_none(),
            },
            max_opt_w,
            palette,
        )?;
    }
    if show_builtin_help {
        write_option_row(
            out,
            OptionRow {
                short: help_short,
                long: "help",
                value_name: None,
                help: "Print help",
                default: None,
                required: false,
            },
            max_opt_w,
            palette,
        )?;
    }
    if show_builtin_version {
        write_option_row(
            out,
            OptionRow {
                short: version_short,
                long: "version",
                value_name: None,
                help: "Print version",
                default: None,
                required: false,
            },
            max_opt_w,
            palette,
        )?;
    }

    Ok(())
}

/// Format a `--help` page for `spec` into a freshly allocated [`String`] in `alloc`.
pub fn format_help_in<A: Allocator + Copy>(
    spec: &CommandSpec<'_>,
    color: bool,
    alloc: A,
) -> Result<String<A>, AllocFailure> {
    let mut buf = String::new_in(alloc);
    let mut writer = TryStringWriter { buf: &mut buf, err: None };
    if format_help_into(spec, color, &mut writer).is_err() {
        let len = writer.buf.len();
        return Err(writer.err.unwrap_or(AllocFailure::CapacityOverflow {
            what: "bstd::cli::format_help_in",
            len,
            additional: 1,
        }));
    }
    Ok(buf)
}

#[derive(Clone, Copy)]
struct HelpPalette {
    heading: Style,
    literal: Style,
    placeholder: Style,
    dim: Style,
}

impl HelpPalette {
    const fn new(color: bool) -> Self {
        if color {
            Self {
                heading: Style::new().bold().fg(Color::Green),
                literal: Style::new().bold().fg(Color::Cyan),
                placeholder: Style::new().fg(Color::Cyan),
                dim: Style::new().dim(),
            }
        } else {
            Self {
                heading: Style::PLAIN,
                literal: Style::PLAIN,
                placeholder: Style::PLAIN,
                dim: Style::PLAIN,
            }
        }
    }
}

#[derive(Clone, Copy)]
struct OptionRow<'a> {
    short: Option<char>,
    long: &'a str,
    value_name: Option<&'a str>,
    help: &'a str,
    default: Option<&'a str>,
    required: bool,
}

struct Bracketed<'a> {
    left: &'a str,
    mid: &'a str,
    right: &'a str,
}

impl fmt::Display for Bracketed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.left)?;
        f.write_str(self.mid)?;
        f.write_str(self.right)
    }
}

struct ShortFlag(char);

impl fmt::Display for ShortFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('-')?;
        f.write_char(self.0)
    }
}

const fn positional_label_len(pos: &PositionalSpec<'_>) -> usize {
    let base = pos.name.len() + 2;
    if pos.multiple { base + 3 } else { base }
}

/// Display width of the option column (excluding the 2-space left margin):
/// `"-c, "` or `"    "` (4) + `"--"` (2) + `long.len()` + optional `" <VALUE>"`.
const fn option_col_len(long: &str, value_name: Option<&str>) -> usize {
    let base = 4 + 2 + long.len();
    match value_name {
        Some(v) => base + 3 + v.len(),
        None => base,
    }
}

fn write_spaces<W: fmt::Write>(out: &mut W, count: usize) -> fmt::Result {
    for _ in 0..count {
        out.write_char(' ')?;
    }
    Ok(())
}

fn write_option_row<W: fmt::Write>(
    out: &mut W,
    row: OptionRow<'_>,
    max_col_w: usize,
    palette: HelpPalette,
) -> fmt::Result {
    out.write_str("  ")?;
    match row.short {
        Some(ch) => write!(out, "{}, ", palette.literal.paint(ShortFlag(ch)))?,
        None => out.write_str("    ")?,
    }
    write!(
        out,
        "{}",
        palette.literal.paint(Bracketed { left: "--", mid: row.long, right: "" })
    )?;
    if let Some(val) = row.value_name {
        write!(
            out,
            " {}",
            palette.placeholder.paint(Bracketed { left: "<", mid: val, right: ">" })
        )?;
    }
    let col_w = option_col_len(row.long, row.value_name);
    let has_right = !row.help.is_empty() || row.default.is_some() || row.required;
    if has_right {
        write_spaces(out, max_col_w.saturating_sub(col_w) + 2)?;
        if !row.help.is_empty() {
            out.write_str(row.help)?;
        }
        if let Some(def) = row.default {
            if !row.help.is_empty() {
                out.write_char(' ')?;
            }
            write!(
                out,
                "{}",
                palette.dim.paint(Bracketed { left: "[default: ", mid: def, right: "]" })
            )?;
        }
        if row.required {
            if !row.help.is_empty() || row.default.is_some() {
                out.write_char(' ')?;
            }
            write!(out, "{}", palette.dim.paint("[required]"))?;
        }
    }
    out.write_char('\n')
}

struct TryStringWriter<'a, A: Allocator> {
    buf: &'a mut String<A>,
    err: Option<AllocFailure>,
}

impl<A: Allocator> fmt::Write for TryStringWriter<'_, A> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.err.is_some() {
            return Err(fmt::Error);
        }
        if let Err(err) = self.buf.try_push_str(s) {
            self.err = Some(err);
            return Err(fmt::Error);
        }
        Ok(())
    }

    fn write_char(&mut self, ch: char) -> fmt::Result {
        if self.err.is_some() {
            return Err(fmt::Error);
        }
        if let Err(err) = self.buf.try_push(ch) {
            self.err = Some(err);
            return Err(fmt::Error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;

    use blaze_own::{Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::{
        CliError, CommandSpec, FlagSpec, OptSpec, ParseOutcome, PositionalSpec, SubcommandSpec,
        format_help_in, parse_in, parse_in_planted,
    };

    const GC_SPEC: CommandSpec<'static> = CommandSpec {
        name: "gc",
        about: "Reclaim unreferenced cache records",
        version: None,
        flags: &[
            FlagSpec { long: "dry-run", short: Some('n'), help: "Show what would be deleted" },
            FlagSpec { long: "force", short: Some('f'), help: "Skip safety confirmation" },
        ],
        opts: &[OptSpec {
            long: "older-than",
            short: Some('t'),
            value_name: "DAYS",
            required: false,
            multiple: false,
            default: Some("14"),
            help: "Only collect records older than DAYS",
        }],
        positionals: &[],
        subcommands: &[],
    };

    const CACHE_SPEC: CommandSpec<'static> = CommandSpec {
        name: "cache",
        about: "Manage the local record store",
        version: None,
        flags: &[FlagSpec { long: "quiet", short: Some('q'), help: "Suppress progress output" }],
        opts: &[],
        positionals: &[],
        subcommands: &[SubcommandSpec {
            name: "gc",
            about: "Reclaim unreferenced records",
            spec: &GC_SPEC,
        }],
    };

    const ROOT_SPEC: CommandSpec<'static> = CommandSpec {
        name: "blaze",
        about: "Fast incremental Rust compiler",
        version: Some("0.2.0"),
        flags: &[
            FlagSpec { long: "verbose", short: Some('v'), help: "Increase verbosity (-vvv)" },
            FlagSpec { long: "all", short: Some('a'), help: "Include all targets" },
            FlagSpec { long: "color", short: Some('c'), help: "Enable ANSI colors" },
        ],
        opts: &[
            OptSpec {
                long: "output",
                short: Some('o'),
                value_name: "PATH",
                required: false,
                multiple: false,
                default: Some("out.bin"),
                help: "Output artifact path",
            },
            OptSpec {
                long: "jobs",
                short: Some('j'),
                value_name: "N",
                required: false,
                multiple: false,
                default: None,
                help: "Worker thread count",
            },
            OptSpec {
                long: "include",
                short: Some('I'),
                value_name: "DIR",
                required: false,
                multiple: true,
                default: None,
                help: "Additional include directory",
            },
        ],
        positionals: &[PositionalSpec {
            name: "FILES",
            required: false,
            multiple: true,
            help: "Input source files",
        }],
        subcommands: &[SubcommandSpec {
            name: "cache",
            about: "Inspect or prune local cache",
            spec: &CACHE_SPEC,
        }],
    };

    const STRICT_SPEC: CommandSpec<'static> = CommandSpec {
        name: "deploy",
        about: "Deploy a single artifact",
        version: Some("1.0.0"),
        flags: &[FlagSpec { long: "verbose", short: Some('v'), help: "Verbose logs" }],
        opts: &[OptSpec {
            long: "target",
            short: Some('t'),
            value_name: "ENV",
            required: true,
            multiple: false,
            default: None,
            help: "Target environment",
        }],
        positionals: &[PositionalSpec {
            name: "ARTIFACT",
            required: true,
            multiple: false,
            help: "Artifact path",
        }],
        subcommands: &[],
    };

    #[test]
    fn flags_short_bundles_and_counts() {
        let h = heap();
        let outcome =
            parse_in(&ROOT_SPEC, &["-vvv", "-ac", "--verbose", "main.rs"], h.alloc())
                .expect("parse");
        let ParseOutcome::Matched(m) = outcome else {
            panic!("expected Matched");
        };
        assert_eq!(m.command_path(), &["blaze"]);
        assert_eq!(m.subcommand(), None);
        assert!(m.flag("verbose"));
        assert_eq!(m.flag_count("verbose"), 4);
        assert!(m.flag("all"));
        assert!(m.flag("color"));
        assert_eq!(m.flag_count("nonexistent"), 0);
        assert_eq!(m.positional(0), Some("main.rs"));
        assert_eq!(m.positionals(), &["main.rs"]);
    }

    #[test]
    fn options_long_short_multiple_default_and_parse() {
        let h = heap();
        let outcome = parse_in(
            &ROOT_SPEC,
            &["--jobs=16", "-Isrc", "-I", "vendor", "-obuild.bin"],
            h.alloc(),
        )
        .expect("parse");
        let ParseOutcome::Matched(m) = outcome else {
            panic!("expected Matched");
        };
        assert_eq!(m.opt("output"), Some("build.bin"));
        assert_eq!(m.opt("jobs"), Some("16"));
        assert_eq!(m.opt_parse::<u32>("jobs"), Ok(Some(16)));
        assert_eq!(m.opt_parse::<u32>("missing"), Ok(None));
        let incs = m.opt_all("include", h.alloc()).expect("opt_all");
        assert_eq!(incs.as_slice(), &["src", "vendor"]);

        // Default option when omitted.
        let def = parse_in(&ROOT_SPEC, &[], h.alloc()).expect("default parse");
        let ParseOutcome::Matched(dm) = def else {
            panic!("expected Matched");
        };
        assert_eq!(dm.opt("output"), Some("out.bin"));
        assert_eq!(dm.opt("jobs"), None);

        // Invalid parse error preserves option name and value.
        let bad = parse_in(&ROOT_SPEC, &["--jobs", "abc"], h.alloc()).expect("parse");
        let ParseOutcome::Matched(bm) = bad else {
            panic!("expected Matched");
        };
        assert_eq!(
            bm.opt_parse::<usize>("jobs"),
            Err(CliError::InvalidValue { opt: "jobs", value: "abc" })
        );
    }

    #[test]
    fn double_dash_terminator_and_nested_subcommands() {
        let h = heap();
        let outcome =
            parse_in(&ROOT_SPEC, &["-v", "--", "--help", "-V", "--unknown", "cache"], h.alloc())
                .expect("parse");
        let ParseOutcome::Matched(m) = outcome else {
            panic!("expected Matched");
        };
        assert_eq!(m.flag_count("verbose"), 1);
        assert_eq!(m.subcommand(), None);
        assert_eq!(m.positionals(), &["--help", "-V", "--unknown", "cache"]);

        // Deep subcommand dispatch (iterative).
        let sub = parse_in(
            &ROOT_SPEC,
            &["-v", "cache", "-q", "gc", "-nf", "--older-than", "30"],
            h.alloc(),
        )
        .expect("subcommand parse");
        let ParseOutcome::Matched(sm) = sub else {
            panic!("expected Matched");
        };
        assert_eq!(sm.command_path(), &["blaze", "cache", "gc"]);
        assert_eq!(sm.subcommand(), Some("gc"));
        assert!(sm.flag("verbose"));
        assert!(sm.flag("quiet"));
        assert!(sm.flag("dry-run"));
        assert!(sm.flag("force"));
        assert_eq!(sm.opt_parse::<u32>("older-than"), Ok(Some(30)));

        // Help on nested subcommand returns that subcommand's spec.
        let help = parse_in(&ROOT_SPEC, &["cache", "gc", "--help"], h.alloc()).expect("help");
        assert_eq!(help, ParseOutcome::Help { command: &GC_SPEC });

        // Version at root.
        let ver = parse_in(&ROOT_SPEC, &["-V"], h.alloc()).expect("version");
        assert_eq!(ver, ParseOutcome::Version { name: "blaze", version: "0.2.0" });
    }

    #[test]
    fn required_unexpected_duplicate_and_typo_suggestions() {
        let h = heap();
        assert_eq!(
            parse_in(&STRICT_SPEC, &["pkg.tar"], h.alloc()),
            Err(CliError::MissingRequiredOpt { opt: "target" })
        );
        assert_eq!(
            parse_in(&STRICT_SPEC, &["--target", "prod"], h.alloc()),
            Err(CliError::MissingRequiredPositional { name: "ARTIFACT" })
        );
        assert_eq!(
            parse_in(&STRICT_SPEC, &["--target", "prod", "a.tar", "b.tar"], h.alloc()),
            Err(CliError::UnexpectedPositional { value: "b.tar" })
        );
        assert_eq!(
            parse_in(&STRICT_SPEC, &["--target", "prod", "--target", "dev", "a.tar"], h.alloc()),
            Err(CliError::DuplicateOpt { opt: "target" })
        );
        assert_eq!(
            parse_in(&STRICT_SPEC, &["--verbose=1"], h.alloc()),
            Err(CliError::UnexpectedValue { flag: "verbose" })
        );
        assert_eq!(
            parse_in(&STRICT_SPEC, &["--target"], h.alloc()),
            Err(CliError::MissingValue { opt: "target" })
        );

        // Typo suggestion for long flag via `crate::diff::edit_distance`.
        let typo = parse_in(&ROOT_SPEC, &["--verbos"], h.alloc());
        assert_eq!(
            typo,
            Err(CliError::UnknownFlag { flag: "verbos", suggestion: Some("verbose") })
        );
        let msg = format_in!(h.alloc(), "{}", typo.expect_err("err"));
        assert!(msg.as_str().contains("did you mean `--verbose`?"));

        // Typo suggestion for subcommand.
        let sub_typo = parse_in(&CACHE_SPEC, &["g"], h.alloc());
        assert_eq!(
            sub_typo,
            Err(CliError::UnknownSubcommand { name: "g", suggestion: Some("gc") })
        );
        let sub_msg = format_in!(h.alloc(), "{}", sub_typo.expect_err("err"));
        assert!(sub_msg.as_str().contains("did you mean `gc`?"));
    }

    #[test]
    fn help_formatting_plain_and_colored() {
        let h = heap();
        let plain = format_help_in(&ROOT_SPEC, false, h.alloc()).expect("plain help");
        assert!(!crate::ansi::has_ansi(plain.as_str()));
        assert!(plain.as_str().contains("Fast incremental Rust compiler"));
        assert!(plain.as_str().contains("Usage: blaze [OPTIONS] [FILES]... <COMMAND>"));
        assert!(plain.as_str().contains("Commands:"));
        assert!(plain.as_str().contains("cache"));
        assert!(plain.as_str().contains("Arguments:"));
        assert!(plain.as_str().contains("Options:"));
        assert!(plain.as_str().contains("-v, --verbose"));
        assert!(plain.as_str().contains("-o, --output <PATH>"));
        assert!(plain.as_str().contains("[default: out.bin]"));
        assert!(plain.as_str().contains("-h, --help"));
        assert!(plain.as_str().contains("-V, --version"));

        let colored = format_help_in(&ROOT_SPEC, true, h.alloc()).expect("colored help");
        assert!(crate::ansi::has_ansi(colored.as_str()));
        let stripped = crate::ansi::strip_in(colored.as_str(), h.alloc()).expect("strip");
        assert_eq!(stripped.as_str(), plain.as_str());
    }

    fn draw_argv<A: Allocator>(tape: &Tape, alloc: A) -> OwnVec<&'static str, A> {
        const POOL: &[&str] = &[
            "-v",
            "-vvv",
            "-ac",
            "--verbose",
            "--verbose=bad",
            "--verbos",
            "--output",
            "--output=foo.o",
            "-o",
            "-ofile.o",
            "-I",
            "-Iinc",
            "--jobs=4",
            "--unknown-flag",
            "-z",
            "-",
            "--",
            "--help",
            "-h",
            "--version",
            "-V",
            "cache",
            "gc",
            "pos1",
            "pos2",
        ];
        let count = tape.int_in(0usize..=10, "argc");
        let mut argv = OwnVec::new_in(alloc);
        for _ in 0..count {
            let pick = tape.index(POOL.len(), "arg");
            argv.push(POOL[pick]);
        }
        argv
    }

    /// The invariant: random argv never panics and every token after `--` is preserved verbatim as a positional argument.
    #[test]
    fn property_random_argv_never_panics_and_double_dash_preserves_tail() {
        let h = heap();
        check(Config::cases(256), |tape| {
            let argv = draw_argv(tape, h.alloc());
            // Arbitrary argv must never panic.
            drop(parse_in(&ROOT_SPEC, argv.as_slice(), h.alloc()));

            // Any tail after `--` on ROOT_SPEC (with no prior error/help/version) becomes positional.
            let tail_len = tape.int_in(0usize..=6, "tail_len");
            const TAIL_POOL: &[&str] =
                &["--help", "-V", "-v", "--output=x", "--unknown", "cache", "-z", "--", "file.rs"];
            let mut full = OwnVec::new_in(h.alloc());
            let mut expected = OwnVec::new_in(h.alloc());
            if tape.coin("prefix_verbose") {
                full.push("-v");
            }
            full.push("--");
            for _ in 0..tail_len {
                let item = TAIL_POOL[tape.index(TAIL_POOL.len(), "tail")];
                full.push(item);
                expected.push(item);
            }
            let outcome = parse_in(&ROOT_SPEC, full.as_slice(), h.alloc()).expect("after --");
            let ParseOutcome::Matched(m) = outcome else {
                panic!("-- must prevent help/version dispatch");
            };
            assert_eq!(m.positionals(), expected.as_slice());
        });
    }

    #[test]
    fn the_oracle_catches_broken_double_dash_separator() {
        let h = heap();
        const TAIL_POOL: &[&str] = &["--help", "-v", "--unknown", "cache", "plain"];
        control(Config::cases(128), false, true, "", |tape, plant| {
            let tail_len = tape.int_in(1usize..=5, "tail_len");
            let mut full = OwnVec::new_in(h.alloc());
            let mut expected = OwnVec::new_in(h.alloc());
            full.push("--");
            for _ in 0..tail_len {
                let item = TAIL_POOL[tape.index(TAIL_POOL.len(), "tail")];
                full.push(item);
                expected.push(item);
            }
            let parsed = parse_in_planted(&ROOT_SPEC, full.as_slice(), h.alloc(), plant)
                .map_err(|e| Broke::Oracle(format_in!(h.alloc(), "unexpected err: {e}").as_str().into()))?;
            let ParseOutcome::Matched(m) = parsed else {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "expected Matched after --, got {parsed:?}")
                        .as_str()
                        .into(),
                ));
            };
            if m.positionals() != expected.as_slice() {
                return Err(Broke::Oracle(
                    format_in!(
                        h.alloc(),
                        "positionals mismatch after --: expected {:?}, got {:?}",
                        expected.as_slice(),
                        m.positionals()
                    )
                    .as_str()
                    .into(),
                ));
            }
            Ok(())
        });
    }
}
