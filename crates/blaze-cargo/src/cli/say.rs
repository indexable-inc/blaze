//! cargo's own lines as text (`core/shell.rs`, `core/compiler/job_queue/mod.rs`,
//! `core/compiler/mod.rs`, `cargo-util`'s `ProcessBuilder` and `ProcessError`): a status
//! word right-aligned in twelve columns, `error:` and `warning:`, the `Finished` line, a unit's
//! name, a command line as cargo quotes it. Every function here is pure; the caller writes the
//! text to the stream cargo writes it to.

use std::{fmt::Write as _, path::Path, time::Duration};

use blaze_api::{TargetKind, UnitTarget, UnitTargetRef};

/// What cargo's lines name a target by: its kind and its name. An owned target and one read in
/// place ([`UnitTargetRef`]) both give it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Named<'a> {
    pub kind: TargetKind,
    pub name: &'a str,
}

impl<'a> From<&'a UnitTarget> for Named<'a> {
    fn from(target: &'a UnitTarget) -> Named<'a> {
        Named { kind: target.kind, name: &target.name }
    }
}

impl<'a> From<UnitTargetRef<'a>> for Named<'a> {
    fn from(target: UnitTargetRef<'a>) -> Named<'a> {
        Named { kind: target.kind, name: target.name }
    }
}

impl<'a> From<&UnitTargetRef<'a>> for Named<'a> {
    fn from(target: &UnitTargetRef<'a>) -> Named<'a> {
        Named { kind: target.kind, name: target.name }
    }
}

/// cargo's `HEADER` and `GOOD` (`util/style.rs`): bright green, bold.
pub const GOOD: &str = "\x1b[1m\x1b[92m";
/// cargo's `ERROR`: annotate-snippets' bright red, bold.
pub const ERROR: &str = "\x1b[1m\x1b[91m";
/// cargo's `WARN` and `INVALID`: annotate-snippets' bright yellow, bold.
pub const WARN: &str = "\x1b[1m\x1b[93m";
/// cargo's `VALID` and `LITERAL`: bright cyan, bold.
pub const VALID: &str = "\x1b[1m\x1b[96m";
/// cargo's `PLACEHOLDER`: cyan.
pub const PLACEHOLDER: &str = "\x1b[36m";
pub const RESET: &str = "\x1b[0m";

/// `text` in `style` when `color`.
pub fn paint(style: &str, text: &str, color: bool) -> String {
    match color {
        true => format!("{style}{text}{RESET}"),
        false => text.to_owned(),
    }
}

/// `   Compiling foo v0.1.0 (/w/foo)`: cargo's `Shell::status`, with its newline.
pub fn status(word: &str, rest: &str, color: bool) -> String {
    let mut line = String::with_capacity(rest.len() + 24);
    status_word(&mut line, word, color);
    line.push_str(rest);
    line.push('\n');
    line
}

/// The start of a [`status`] line in `out`: `word` right-aligned in twelve columns, then a space.
pub fn status_word(out: &mut String, word: &str, color: bool) {
    if color {
        out.push_str(GOOD);
    }
    out.extend(std::iter::repeat_n(' ', 12usize.saturating_sub(word.len())));
    out.push_str(word);
    if color {
        out.push_str(RESET);
    }
    out.push(' ');
}

/// `level: what` and its newline into `out`, as cargo-util-terminal's `message_stderr` writes an
/// unjustified status: the level in `style`, the colon plain.
fn leveled(out: &mut String, style: &str, level: &str, what: &str, color: bool) {
    leveled_with(out, style, level, color, |out| out.push_str(what));
}

/// [`leveled`] of the text `what` writes into `out`.
pub fn leveled_with(out: &mut String, style: &str, level: &str, color: bool, what: impl FnOnce(&mut String)) {
    if color {
        out.push_str(style);
    }
    out.push_str(level);
    if color {
        out.push_str(RESET);
    }
    out.push_str(": ");
    what(out);
    out.push('\n');
}

/// `error: what`: cargo's `Shell::error`, with its newline, into `out`.
pub fn error_into(out: &mut String, what: &str, color: bool) {
    leveled(out, ERROR, "error", what, color);
}

/// `warning: what`: cargo's `Shell::warn`, with its newline, into `out`.
pub fn warning_into(out: &mut String, what: &str, color: bool) {
    leveled(out, WARN, "warning", what, color);
}

/// `note: what`: cargo's `Shell::note` (annotate-snippets' secondary title: `note` in its note
/// style, the colon and the text plain), with its newline, into `out`.
pub fn note_into(out: &mut String, what: &str, color: bool) {
    leveled(out, GOOD, "note", what, color);
}

/// [`error_into`] as its own text, for an error path.
pub fn error(what: &str, color: bool) -> String {
    let mut out = String::with_capacity(what.len() + 24);
    error_into(&mut out, what, color);
    out
}

/// [`warning_into`] as its own text, for a path off the per-response one.
pub fn warning(what: &str, color: bool) -> String {
    let mut out = String::with_capacity(what.len() + 24);
    warning_into(&mut out, what, color);
    out
}

/// A time as cargo's `util::elapsed` writes it: `0.12s`, `1m 05s`, into `out`.
pub fn elapsed_into(out: &mut String, took: Duration) {
    let secs = took.as_secs();
    // Writing to a String cannot fail.
    let _ = match secs >= 60 {
        true => write!(out, "{}m {:02}s", secs / 60, secs % 60),
        false => write!(out, "{secs}.{:02}s", took.subsec_nanos() / 10_000_000),
    };
}

/// [`elapsed_into`] as its own text.
pub fn elapsed(took: Duration) -> String {
    let mut out = String::with_capacity(12);
    elapsed_into(&mut out, took);
    out
}

/// What follows `Finished`: `` `dev` profile [unoptimized + debuginfo] target(s) in 0.12s ``,
/// of the daemon's `profile` (`Response::Built`), into `out`.
pub fn finished_into(out: &mut String, profile: &str, took: Duration) {
    out.push_str(profile);
    out.push_str(" target(s) in ");
    elapsed_into(out, took);
}

/// [`finished_into`] as its own text.
pub fn finished(profile: &str, took: Duration) -> String {
    let mut out = String::with_capacity(profile.len() + 24);
    finished_into(&mut out, profile, took);
    out
}

/// A package as cargo's status lines name it: `foo v0.1.0 (/w/foo)`, `serde v1.0.0`.
pub fn package(name: &str, version: &str, source: Option<&str>) -> String {
    match source {
        Some(source) => format!("{name} v{version} {source}"),
        None => format!("{name} v{version}"),
    }
}

/// A target as cargo's `Target::description_named` names it: `lib`, `bin "foo"`,
/// `build script`.
pub fn target_named<'a>(target: impl Into<Named<'a>>) -> String {
    let target = target.into();
    let mut out = String::with_capacity(target.name.len() + 12);
    target_named_into(&mut out, target);
    out
}

/// [`target_named`] into `out`.
pub fn target_named_into<'a>(out: &mut String, target: impl Into<Named<'a>>) {
    let target = target.into();
    let kind = match target.kind {
        TargetKind::CustomBuild => return out.push_str("build script"),
        TargetKind::Lib => return out.push_str("lib"),
        kind @ (TargetKind::Bin | TargetKind::Test | TargetKind::Bench | TargetKind::Example) => kind.name(),
    };
    out.push_str(kind);
    out.push_str(" \"");
    out.push_str(target.name);
    out.push('"');
}

/// Whether a unit compiled with `--test` is named a test of its target: it is no test or bench
/// target (cargo's `mode.is_rustc_test() && !(target.is_test() || target.is_bench())`).
fn test_of(target: Named<'_>, test: bool) -> bool {
    test && !matches!(target.kind, TargetKind::Test | TargetKind::Bench)
}

/// A unit as cargo's `descriptive_pkg_name` names it: `` `foo` (lib) ``, `` `foo` (lib test) ``
/// for one compiled with `--test` that is no test or bench target.
pub fn unit_named<'a>(package: &str, target: impl Into<Named<'a>>, test: bool) -> String {
    let target = target.into();
    let mut out = String::with_capacity(package.len() + target.name.len() + 20);
    unit_named_into(&mut out, package, target, test);
    out
}

/// [`unit_named`] into `out`.
pub fn unit_named_into<'a>(out: &mut String, package: &str, target: impl Into<Named<'a>>, test: bool) {
    let target = target.into();
    out.push('`');
    out.push_str(package);
    out.push_str("` (");
    target_named_into(out, target);
    if test_of(target, test) {
        out.push_str(" test");
    }
    out.push(')');
}

/// The package name in cargo's package id: `path+file:///w/foo#0.1.0` is `foo`,
/// `registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0` is `serde`.
pub fn package_name(package_id: &str) -> &str {
    let (url, fragment) = package_id.split_once('#').unwrap_or((package_id, ""));
    if let Some((name, _version)) = fragment.split_once('@') {
        return name;
    }
    let url = url.split(['?', '#']).next().unwrap_or(url);
    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url)
}

/// The version in cargo's package id: the part after `@`, or the whole fragment.
pub fn package_version(package_id: &str) -> &str {
    let fragment = package_id.split_once('#').map_or("", |(_, it)| it);
    fragment.split_once('@').map_or(fragment, |(_, version)| version)
}

/// What a unit's warnings came to, once it ended (`JobQueue::report_warning_count`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Warned {
    pub total: u32,
    pub duplicates: u32,
    /// The warnings with a machine-applicable suggestion; `None` once the unit has an error,
    /// which takes `cargo fix`'s hint away.
    pub fixable: Option<u32>,
}

/// `` `foo` (lib) generated 2 warnings (1 duplicate) (run `cargo fix --lib -p foo` to apply 1
/// suggestion) ``: what follows `warning:` for a unit with warnings, `None` for one without
/// (`JobQueue::report_warning_count`). The hint is for a local unit that is no build script,
/// and always names the package (cargo since "fix(check): ... always using -p").
pub fn generated<'a>(package: &str, target: impl Into<Named<'a>>, test: bool, local: bool, warned: Warned) -> Option<String> {
    let target = target.into();
    let mut out = String::with_capacity(package.len() * 2 + target.name.len() + 96);
    generated_into(&mut out, package, target, test, local, warned).then_some(out)
}

/// [`generated`] into `out`; false, with nothing written, for a unit without warnings.
pub fn generated_into<'a>(
    out: &mut String,
    package: &str,
    target: impl Into<Named<'a>>,
    test: bool,
    local: bool,
    warned: Warned,
) -> bool {
    if warned.total == 0 {
        return false;
    }
    let target = target.into();
    unit_named_into(out, package, target, test);
    // Writing to a String cannot fail.
    let _ = match warned.total {
        1 => write!(out, " generated 1 warning"),
        n => write!(out, " generated {n} warnings"),
    };
    let _ = match warned.duplicates {
        0 => Ok(()),
        1 => write!(out, " (1 duplicate)"),
        n => write!(out, " ({n} duplicates)"),
    };
    if let Some(fixable @ 1..) = warned.fixable
        && local
        && target.kind != TargetKind::CustomBuild
    {
        out.push_str(" (run `cargo fix --");
        target_named_into(out, target);
        out.push_str(" -p ");
        out.push_str(package);
        if test_of(target, test) {
            out.push_str(" --tests");
        }
        let _ = match fixable {
            1 => write!(out, "` to apply 1 suggestion)"),
            n => write!(out, "` to apply {n} suggestions)"),
        };
    }
    true
}

/// `` could not compile `foo` (lib) due to 1 previous error; 1 warning emitted ``: what follows
/// `error:` for a unit that failed (`core/compiler/mod.rs`, `rustc`).
pub fn could_not_compile<'a>(package: &str, target: impl Into<Named<'a>>, test: bool, errors: u32, warnings: u32) -> String {
    let target = target.into();
    let mut out = String::with_capacity(package.len() + target.name.len() + 80);
    could_not_compile_into(&mut out, package, target, test, errors, warnings);
    out
}

/// [`could_not_compile`] into `out`.
pub fn could_not_compile_into<'a>(
    out: &mut String,
    package: &str,
    target: impl Into<Named<'a>>,
    test: bool,
    errors: u32,
    warnings: u32,
) {
    out.push_str("could not compile ");
    unit_named_into(out, package, target, test);
    // Writing to a String cannot fail.
    let _ = match errors {
        0 => Ok(()),
        1 => write!(out, " due to 1 previous error"),
        n => write!(out, " due to {n} previous errors"),
    };
    let _ = match warnings {
        0 => Ok(()),
        1 => write!(out, "; 1 warning emitted"),
        n => write!(out, "; {n} warnings emitted"),
    };
}

/// `arg` as cargo-util's `ProcessBuilder` shows it, quoted as `shell-escape` quotes it.
pub fn escape(arg: &str) -> String {
    #[cfg(windows)]
    {
        escape_windows(arg)
    }
    #[cfg(not(windows))]
    {
        escape_unix(arg)
    }
}

/// `shell_escape::unix::escape`.
pub fn escape_unix(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '/' | ',' | '.' | '+');
    if !arg.is_empty() && arg.chars().all(plain) {
        return arg.to_owned();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        match c {
            '\'' | '!' => {
                quoted.push_str("'\\");
                quoted.push(c);
                quoted.push('\'');
            }
            _ => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// `shell_escape::windows::escape`.
pub fn escape_windows(arg: &str) -> String {
    let needs = arg.is_empty() || arg.chars().any(|c| matches!(c, '"' | '\t' | '\n' | ' '));
    if !needs {
        return arg.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut chars = arg.chars().peekable();
    loop {
        let mut slashes = 0;
        while chars.next_if_eq(&'\\').is_some() {
            slashes += 1;
        }
        match chars.next() {
            Some('"') => {
                quoted.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                quoted.push('"');
            }
            Some(c) => {
                quoted.extend(std::iter::repeat_n('\\', slashes));
                quoted.push(c);
            }
            None => {
                quoted.extend(std::iter::repeat_n('\\', slashes * 2));
                break;
            }
        }
    }
    quoted.push('"');
    quoted
}

/// `` `target/debug/foo a 'b c'` ``: a command as cargo's status lines show it, without the
/// variables cargo set for it.
pub fn command_line(program: &str, args: &[String]) -> String {
    command_line_with(&[], program, args)
}

/// [`command_line`] with the variables cargo set for it first, sorted by name, as `-vv` shows
/// them (`ProcessBuilder::display_env_vars`): `` `CARGO=x FOO='a b' target/debug/foo` ``, and
/// `set K=V&& ` before each on Windows. Only cargo's own variables are ever here, never the
/// user's environment.
pub fn command_line_with(env: &[(String, String)], program: &str, args: &[String]) -> String {
    let mut line = String::with_capacity(program.len() + 2 + args.iter().map(|it| it.len() + 3).sum::<usize>());
    line.push('`');
    let mut sorted: Vec<&(String, String)> = env.iter().collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in sorted {
        if cfg!(windows) {
            line.push_str("set ");
        }
        line.push_str(name);
        line.push('=');
        line.push_str(&escape(value));
        line.push_str(if cfg!(windows) { "&& " } else { " " });
    }
    line.push_str(program);
    for arg in args {
        line.push(' ');
        line.push_str(&escape(arg));
    }
    line.push('`');
    line
}

/// A path as cargo shows one it runs: from `cwd` when it is below it.
pub fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// How a program cargo ran ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    Code(i32),
    /// Killed by the signal (Unix).
    Signal(i32),
}

impl Ended {
    pub fn of(status: std::process::ExitStatus) -> Ended {
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Ended::Signal(signal);
        }
        Ended::Code(status.code().unwrap_or(1))
    }

    pub fn success(self) -> bool {
        self == Ended::Code(0)
    }
}

/// `exit status: 1`, `signal: 11, SIGSEGV: invalid memory reference`: cargo-util's
/// `status_to_string`.
pub fn ended(ended: Ended) -> String {
    match ended {
        // alloc: a message for the terminal, once per process that ended
        Ended::Signal(signal) => format!("signal: {signal}{}", blaze_core::signal::described(signal)),
        Ended::Code(code) => code_named(code),
    }
}

#[cfg(not(windows))]
fn code_named(code: i32) -> String {
    format!("exit status: {code}")
}

/// std's `exit code: 0xc0000005` for an NTSTATUS, with cargo's name for it.
#[cfg(windows)]
fn code_named(code: i32) -> String {
    let status = code as u32;
    if status & 0x8000_0000 == 0 {
        return format!("exit code: {code}");
    }
    let name = match status {
        0xC000_0005 => "STATUS_ACCESS_VIOLATION",
        0xC000_0006 => "STATUS_IN_PAGE_ERROR",
        0xC000_0008 => "STATUS_INVALID_HANDLE",
        0xC000_000D => "STATUS_INVALID_PARAMETER",
        0xC000_0017 => "STATUS_NO_MEMORY",
        0xC000_001D => "STATUS_ILLEGAL_INSTRUCTION",
        0xC000_0025 => "STATUS_NONCONTINUABLE_EXCEPTION",
        0xC000_0026 => "STATUS_INVALID_DISPOSITION",
        0xC000_008C => "STATUS_ARRAY_BOUNDS_EXCEEDED",
        0xC000_008D => "STATUS_FLOAT_DENORMAL_OPERAND",
        0xC000_008E => "STATUS_FLOAT_DIVIDE_BY_ZERO",
        0xC000_008F => "STATUS_FLOAT_INEXACT_RESULT",
        0xC000_0090 => "STATUS_FLOAT_INVALID_OPERATION",
        0xC000_0091 => "STATUS_FLOAT_OVERFLOW",
        0xC000_0092 => "STATUS_FLOAT_STACK_CHECK",
        0xC000_0093 => "STATUS_FLOAT_UNDERFLOW",
        0xC000_0094 => "STATUS_INTEGER_DIVIDE_BY_ZERO",
        0xC000_0095 => "STATUS_INTEGER_OVERFLOW",
        0xC000_0096 => "STATUS_PRIVILEGED_INSTRUCTION",
        0xC000_00FD => "STATUS_STACK_OVERFLOW",
        0xC000_0135 => "STATUS_DLL_NOT_FOUND",
        0xC000_0138 => "STATUS_ORDINAL_NOT_FOUND",
        0xC000_0139 => "STATUS_ENTRYPOINT_NOT_FOUND",
        0xC000_013A => "STATUS_CONTROL_C_EXIT",
        0xC000_0142 => "STATUS_DLL_INIT_FAILED",
        0xC000_02B4 => "STATUS_FLOAT_MULTIPLE_FAULTS",
        0xC000_02B5 => "STATUS_FLOAT_MULTIPLE_TRAPS",
        0xC000_02C9 => "STATUS_REG_NAT_CONSUMPTION",
        0xC000_0374 => "STATUS_HEAP_CORRUPTION",
        0xC000_0409 => "STATUS_STACK_BUFFER_OVERRUN",
        0xC000_0420 => "STATUS_ASSERTION_FAILURE",
        _ => return format!("exit code: {status:#x}"),
    };
    format!("exit code: {status:#x}, {name}")
}

/// `text` without the escapes that color a terminal (`anstream::adapter::strip_str`): CSI
/// sequences to their final byte, OSC sequences to BEL or ST.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests;
