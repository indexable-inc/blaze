//! Snapshot tests, in place of the expect-test crate and with its behaviour: `expect![[r#"..."#]]`
//! holds the expected text in the test's own source, `expect_file!["x.txt"]` in a file beside
//! it, and a run with `UPDATE_EXPECT=1` set rewrites each one that differs to what the test saw
//! instead of failing.
//!
//! An inline literal is read as expect-test reads it: a text of one line is taken as written; a
//! text of more lines loses its first newline and the common indent of its non-blank lines
//! ([`trim_indent`]). An update writes the literal back in the same shape ([`format_patch`]):
//! a raw string with one more `#` than any `"#..` in the text, its lines indented four spaces
//! past the line of the `expect!`, unless the test said `indent(false)`.
//!
//! The parts that read and write text are pure functions over strings ([`locate`],
//! [`format_patch`], [`Patchwork`]), so `tests` checks the round trip from a text to a literal
//! and back; only [`Expect::assert_eq`] and [`ExpectFile::assert_eq`] touch the disk.

use std::{
    fmt, fs,
    ops::Range,
    panic,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use blaze_core::lock::Lock;

const HELP: &str = "
You can update all `expect!` tests by running:

    env UPDATE_EXPECT=1 cargo test

To update a single test, place the cursor on `expect` token and use `run` feature of rust-analyzer.
";

fn update_expect() -> bool {
    std::env::var_os("UPDATE_EXPECT").is_some()
}

/// An expected text written in the test's source: `expect![[r#"..."#]]`, `expect!["..."]` or
/// `expect![[]]`.
#[macro_export]
macro_rules! expect {
    [$data:literal] => { $crate::expect![[$data]] };
    [[$data:literal]] => { $crate::Expect {
        position: $crate::expect::Position { file: file!(), line: line!(), column: column!() },
        data: $data,
        indent: true,
    } };
    [] => { $crate::expect![[""]] };
    [[]] => { $crate::expect![[""]] };
}

/// An expected text in a file, its path relative to the test's source file.
#[macro_export]
macro_rules! expect_file {
    [$path:expr] => { $crate::ExpectFile {
        path: std::path::PathBuf::from($path),
        position: file!(),
    } };
}

/// What `expect!` makes; see the module's text.
#[derive(Debug)]
pub struct Expect {
    #[doc(hidden)]
    pub position: Position,
    #[doc(hidden)]
    pub data: &'static str,
    #[doc(hidden)]
    pub indent: bool,
}

/// What `expect_file!` makes; see the module's text.
#[derive(Debug)]
pub struct ExpectFile {
    #[doc(hidden)]
    pub path: PathBuf,
    #[doc(hidden)]
    pub position: &'static str,
}

/// Where an `expect!` is written: `file!()`, `line!()` and `column!()` of its call.
#[derive(Debug, Clone, Copy)]
pub struct Position {
    pub file: &'static str,
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

impl Expect {
    /// Passes when `actual` is the expected text; else fails the test with a diff, or under
    /// `UPDATE_EXPECT` rewrites the literal to `actual`.
    #[track_caller]
    pub fn assert_eq(&self, actual: &str) {
        let trimmed = self.trimmed();
        if trimmed == actual {
            return;
        }
        if update_expect() {
            eprintln!("\x1b[1m\x1b[92mupdating\x1b[0m: {}", self.position);
            update_inline(self, actual);
            return;
        }
        fail(self.position.to_string(), &trimmed, actual);
    }

    /// [`Expect::assert_eq`] of `actual`'s `{:#?}` form and a newline.
    #[track_caller]
    pub fn assert_debug_eq(&self, actual: &impl fmt::Debug) {
        self.assert_eq(&format!("{actual:#?}\n"));
    }

    /// Whether an update indents the new literal past the line of the `expect!` (the default).
    pub fn indent(&mut self, yes: bool) {
        self.indent = yes;
    }

    /// The literal as written, before any trimming.
    pub fn data(&self) -> &str {
        self.data
    }

    fn trimmed(&self) -> String {
        trimmed(self.data)
    }
}

/// The expected text a literal holds: as written when it is one line, else [`trim_indent`].
pub fn trimmed(data: &str) -> String {
    if !data.contains('\n') {
        return data.to_owned();
    }
    trim_indent(data)
}

impl ExpectFile {
    /// Passes when `actual` is the file's text; else fails the test with a diff, or under
    /// `UPDATE_EXPECT` writes `actual` to the file.
    #[track_caller]
    pub fn assert_eq(&self, actual: &str) {
        let expected = self.data();
        if actual == expected {
            return;
        }
        if update_expect() {
            eprintln!("\x1b[1m\x1b[92mupdating\x1b[0m: {}", self.path.display());
            let path = self.abs_path();
            if let Err(error) = fs::write(&path, actual) {
                panic!("can't write {}: {error}", path.display());
            }
            return;
        }
        fail(self.path.display().to_string(), &expected, actual);
    }

    /// [`ExpectFile::assert_eq`] of `actual`'s `{:#?}` form and a newline.
    #[track_caller]
    pub fn assert_debug_eq(&self, actual: &impl fmt::Debug) {
        self.assert_eq(&format!("{actual:#?}\n"));
    }

    /// The file's text with `\r\n` read as `\n`; a file that is not there reads as empty, so a
    /// new snapshot is made by an update.
    pub fn data(&self) -> String {
        fs::read_to_string(self.abs_path()).unwrap_or_default().replace("\r\n", "\n")
    }

    fn abs_path(&self) -> PathBuf {
        if self.path.is_absolute() {
            return self.path.clone();
        }
        let Some(dir) = Path::new(self.position).parent() else {
            panic!("`expect_file!` in {} has no directory to read from", self.position)
        };
        to_abs_ws_path(&dir.join(&self.path))
    }
}

/// `file!()` paths are relative to the workspace root, which is two levels above this crate's
/// manifest (`crates/test-utils`).
fn to_abs_ws_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_owned();
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(root) = manifest.parent().and_then(Path::parent) else {
        panic!("{} is not two levels below a workspace root", manifest.display())
    };
    root.join(path)
}

/// Whether the help has been printed in this process: once is enough.
static HELP_PRINTED: AtomicBool = AtomicBool::new(false);

/// The source files rewritten in this process, each with the patches made to it so far: a
/// second update in one file finds its literal by the line it had in the original text.
static FILES: Lock<Vec<FileRuntime>> = Lock::new("test_utils::expect.files", Vec::new());

struct FileRuntime {
    file: &'static str,
    path: PathBuf,
    original_text: String,
    patchwork: Patchwork,
}

fn update_inline(expect: &Expect, actual: &str) {
    let mut files = FILES.lock();
    let at = match files.iter().position(|it| it.file == expect.position.file) {
        Some(at) => at,
        None => {
            let path = to_abs_ws_path(Path::new(expect.position.file));
            let original_text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => panic!("can't read {} to update it: {error}", path.display()),
            };
            let patchwork = Patchwork::new(original_text.clone());
            files.push(FileRuntime { file: expect.position.file, path, original_text, patchwork });
            files.len() - 1
        }
    };
    let file = &mut files[at];
    let loc = locate(&file.original_text, expect.position);
    let patch = format_patch(expect.indent.then_some(loc.line_indent), actual);
    file.patchwork.patch(loc.literal_range, &patch);
    if let Err(error) = fs::write(&file.path, &file.patchwork.text) {
        panic!("can't write {}: {error}", file.path.display());
    }
}

fn fail(position: String, expected: &str, actual: &str) -> ! {
    let help = if HELP_PRINTED.swap(true, Ordering::Relaxed) { "" } else { HELP };
    let diff = crate::format_diff(dissimilar::diff(expected, actual));
    eprintln!(
        "\n
\x1b[1m\x1b[91merror\x1b[97m: expect test failed\x1b[0m
   \x1b[1m\x1b[34m-->\x1b[0m {position}
{help}
\x1b[1mExpect\x1b[0m:
----
{expected}
----

\x1b[1mActual\x1b[0m:
----
{actual}
----

\x1b[1mDiff\x1b[0m:
----
{diff}
----
"
    );
    // `resume_unwind`, not `panic!`: a backtrace would be noise after the diff.
    panic::resume_unwind(Box::new(()))
}

/// Where a literal is in a source text, and the indent of the line the `expect!` is on.
#[derive(Debug, PartialEq, Eq)]
pub struct Location {
    pub line_indent: usize,
    /// The bytes to replace: the literal, with the `[`..`]` around it when written `[[..]]`.
    pub literal_range: Range<usize>,
}

/// The literal of the `expect!` at `position` in `file`, the text `position.file` had when the
/// test was compiled. Panics when there is no `expect!` there.
pub fn locate(file: &str, position: Position) -> Location {
    let mut target = None;
    let mut line_start = 0;
    for (i, line) in lines_with_ends(file).enumerate() {
        if i + 1 == position.line as usize {
            // `column` is the first character of the call: past it, the `!`, blanks and the
            // macro's delimiter is the argument.
            let mut chars = line.char_indices().skip(position.column.saturating_sub(1) as usize);
            let found = chars
                .by_ref()
                .find(|&(_, c)| c == '!')
                .and_then(|_| chars.by_ref().find(|&(_, c)| !c.is_whitespace()))
                .and_then(|_| chars.find(|&(_, c)| !c.is_whitespace()));
            let Some((byte_offset, _)) = found else {
                panic!("no `expect!` call at {position}")
            };
            let indent = line.chars().take_while(|&it| it == ' ').count();
            target = Some((line_start + byte_offset, indent));
            break;
        }
        line_start += line.len();
    }
    let Some((literal_start, line_indent)) = target else {
        panic!("{position} is past the end of the file")
    };
    let lit_to_eof = &file[literal_start..];
    let lit_to_eof_trimmed = lit_to_eof.trim_start();
    let literal_start = literal_start + (lit_to_eof.len() - lit_to_eof_trimmed.len());
    let Some(literal_len) = locate_end(lit_to_eof_trimmed) else {
        panic!("no closing delimiter for the `expect!` at {position}")
    };
    Location { line_indent, literal_range: literal_start..literal_start + literal_len }
}

/// The length of the argument starting `arg_start_to_eof`: `[` literal `]`, a literal, or none.
fn locate_end(arg_start_to_eof: &str) -> Option<usize> {
    match arg_start_to_eof.chars().next()? {
        '[' => {
            let str_start_to_eof = arg_start_to_eof[1..].trim_start();
            let str_len = find_str_lit_len(str_start_to_eof)?;
            let str_end_to_eof = &str_start_to_eof[str_len..];
            let closing_brace_offset = str_end_to_eof.find(']')?;
            Some((arg_start_to_eof.len() - str_end_to_eof.len()) + closing_brace_offset + 1)
        }
        // `expect![]`, `expect!{}`, `expect!()`.
        ']' | '}' | ')' => Some(0),
        _ => find_str_lit_len(arg_start_to_eof),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StrLitKind {
    Normal,
    /// A raw string with this many `#`.
    Raw(usize),
}

/// The length of the string literal (`"..."` or `r#"..."#`) that starts `text`.
fn find_str_lit_len(text: &str) -> Option<usize> {
    let mut s = text.chars();
    let kind = match s.next()? {
        '"' => StrLitKind::Normal,
        'r' => {
            let mut hashes = 0;
            loop {
                match s.next()? {
                    '#' => hashes += 1,
                    '"' => break StrLitKind::Raw(hashes),
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    loop {
        match (s.next()?, kind) {
            ('\\', StrLitKind::Normal) => {
                s.next()?;
            }
            ('"', StrLitKind::Normal) => break,
            // A `"` closes a raw literal when `n` `#` follow it; else what follows is read on.
            ('"', StrLitKind::Raw(n)) => {
                if s.as_str().as_bytes().get(..n).is_some_and(|it| it.iter().all(|&b| b == b'#')) {
                    s = s.as_str()[n..].chars();
                    break;
                }
            }
            _ => {}
        }
    }
    Some(text.len() - s.as_str().len())
}

/// Edits to a text given by ranges of its original, made one at a time in any order.
#[derive(Debug)]
pub struct Patchwork {
    pub text: String,
    /// Each edit made: the original range deleted and the length inserted for it.
    indels: Vec<(Range<usize>, usize)>,
}

impl Patchwork {
    pub fn new(text: String) -> Patchwork {
        Patchwork { text, indels: Vec::new() }
    }

    /// Replaces `range` of the original text, which no earlier edit overlapped, by `patch`.
    pub fn patch(&mut self, mut range: Range<usize>, patch: &str) {
        self.indels.push((range.clone(), patch.len()));
        self.indels.sort_by_key(|(delete, _insert)| delete.start);
        let (delete, insert) = self
            .indels
            .iter()
            .take_while(|(delete, _)| delete.start < range.start)
            .fold((0usize, 0usize), |(d, i), (delete, insert)| (d + delete.len(), i + insert));
        range.start = range.start - delete + insert;
        range.end = range.end - delete + insert;
        self.text.replace_range(range, patch);
    }
}

#[cfg(test)]
thread_local! {
    /// The planted bug of the round-trip control: one `#` fewer than a raw literal needs.
    pub(crate) static TOO_FEW_HASHES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The `#` a raw literal of `patch` needs past the most that follow a `"` in it.
fn hashes_past(most: usize) -> usize {
    #[cfg(test)]
    if TOO_FEW_HASHES.with(std::cell::Cell::get) {
        return most;
    }
    most + 1
}

fn lit_kind_for_patch(patch: &str) -> StrLitKind {
    if !patch.contains('"') {
        let has_bslash_or_newline = patch.chars().any(|c| matches!(c, '\\' | '\n'));
        return if has_bslash_or_newline { StrLitKind::Raw(1) } else { StrLitKind::Normal };
    }
    let leading_hashes = |s: &str| s.chars().take_while(|&c| c == '#').count();
    let most = patch.split('"').map(leading_hashes).max().unwrap_or(0);
    StrLitKind::Raw(hashes_past(most))
}

/// The literal an update writes for `patch`: `"text"` when nothing in it needs escaping, else
/// `[r#"..."#]` with enough `#`; a text of more lines starts on a new line, each non-blank line
/// indented `desired_indent + 4` and the closing quote at `desired_indent`.
pub fn format_patch(desired_indent: Option<usize>, patch: &str) -> String {
    let kind = lit_kind_for_patch(patch);
    let indent = desired_indent.map(|it| " ".repeat(it));
    let is_multiline = patch.contains('\n');

    let mut buf = String::new();
    if let StrLitKind::Raw(n) = kind {
        buf.push_str("[r");
        buf.extend(std::iter::repeat_n('#', n));
    }
    buf.push('"');
    if is_multiline {
        buf.push('\n');
    }
    let mut final_newline = false;
    for line in lines_with_ends(patch) {
        if is_multiline
            && !line.trim().is_empty()
            && let Some(indent) = &indent
        {
            buf.push_str(indent);
            buf.push_str("    ");
        }
        buf.push_str(line);
        final_newline = line.ends_with('\n');
    }
    if final_newline && let Some(indent) = &indent {
        buf.push_str(indent);
    }
    buf.push('"');
    if let StrLitKind::Raw(n) = kind {
        buf.extend(std::iter::repeat_n('#', n));
        buf.push(']');
    }
    buf
}

/// `text` without its first newline and the common indent of its non-blank lines; a line no
/// longer than that indent loses its leading spaces only.
pub fn trim_indent(mut text: &str) -> String {
    if let Some(rest) = text.strip_prefix('\n') {
        text = rest;
    }
    let indent = text
        .lines()
        .filter(|it| !it.trim().is_empty())
        .map(|it| it.len() - it.trim_start().len())
        .min()
        .unwrap_or(0);
    lines_with_ends(text)
        .map(|line| if line.len() <= indent { line.trim_start_matches(' ') } else { &line[indent..] })
        .collect()
}

/// The lines of `text`, each with its `\n`.
fn lines_with_ends(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let at = rest.find('\n').map_or(rest.len(), |it| it + 1);
        let (line, next) = rest.split_at(at);
        rest = next;
        Some(line)
    })
}

/// The text a literal written by [`format_patch`] (or by hand) holds, read as the compiler
/// reads a raw or plain literal without escapes: the tests' reader, since an update never
/// writes an escape.
#[cfg(test)]
fn literal_text(literal: &str) -> Option<&str> {
    let inner = literal.strip_prefix('[').and_then(|it| it.strip_suffix(']')).unwrap_or(literal);
    let len = find_str_lit_len(inner)?;
    if len != inner.len() {
        return None;
    }
    match inner.strip_prefix('r') {
        Some(raw) => {
            let hashes = raw.len() - raw.trim_start_matches('#').len();
            raw.get(hashes + 1..raw.len() - hashes - 1)
        }
        None => inner.get(1..inner.len() - 1),
    }
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape};

    use super::{
        Patchwork, Position, TOO_FEW_HASHES, format_patch, literal_text, locate, trim_indent,
        trimmed,
    };

    #[test]
    fn trim_indent_drops_the_first_newline_and_the_common_indent() {
        assert_eq!(trim_indent("\n    a\n      b\n\n    c\n  "), "a\n  b\n\nc\n");
        assert_eq!(trim_indent("  a\n b\n"), " a\nb\n");
        assert_eq!(trimmed("  one line  "), "  one line  ");
        assert_eq!(trimmed(""), "");
    }

    /// A text with no line that only has blanks before its end, at least one line starting
    /// with no blank, and every character an update has to quote.
    fn snapshot(tape: &Tape) -> String {
        let lines = tape.below(4, "lines") + 1;
        let mut text = String::new();
        for at in 0..lines {
            if at > 0 {
                text.push('\n');
            }
            let blank = at > 0 && tape.one_in(4, "blank");
            if blank {
                continue;
            }
            text.push_str(&tape.text_from("a\"#\\r", 1..=1, "first"));
            if at > 0 && tape.coin("indented") {
                text.insert_str(text.len() - 1, "  ");
            }
            text.push_str(&tape.text_from("a \"#\\r", 0..=8, "rest"));
        }
        if tape.coin("ends with a newline") {
            text.push('\n');
        }
        text
    }

    /// What `expect!` reads from the literal an update writes for `actual`.
    fn round_trip(tape: &Tape) -> Result<(), Broke> {
        let actual = snapshot(tape);
        let indent = if tape.coin("indent") { Some(tape.index(9, "line indent")) } else { None };
        let literal = format_patch(indent, &actual);
        let Some(data) = literal_text(&literal) else {
            return Err(Broke::Oracle(format!(
                "the literal {literal:?} written for {actual:?} ends before its end"
            )));
        };
        let read = trimmed(data);
        if read != actual {
            return Err(Broke::Oracle(format!(
                "the literal {literal:?} written for {actual:?} reads back as {read:?}"
            )));
        }
        Ok(())
    }

    /// The invariant: the literal an update writes reads back, through `expect!`'s trimming,
    /// as the text it was written for.
    #[test]
    fn an_updated_literal_reads_back_as_the_actual_text() {
        blaze_sim::check(Config::cases(500), round_trip);
    }

    #[derive(Clone, Copy, Debug)]
    enum Plant {
        Clean,
        TooFewHashes,
    }

    #[test]
    fn the_oracle_catches_too_few_hashes() {
        let run = |tape: &Tape, plant: Plant| {
            TOO_FEW_HASHES.with(|it| it.set(matches!(plant, Plant::TooFewHashes)));
            let ran = round_trip(tape);
            TOO_FEW_HASHES.with(|it| it.set(false));
            ran
        };
        let config = Config::cases(500);
        blaze_sim::control(config, Plant::Clean, Plant::TooFewHashes, "written for", run);
    }

    /// Two `expect!`s in one file, the first growing by lines: the second is found by its line
    /// in the original text and patched where it is now.
    #[test]
    fn two_updates_in_one_file_patch_both_literals() {
        let file = "fn t() {\n    expect![[r#\"\n        old\n    \"#]].assert_eq(&a);\n    expect![\"x\"].assert_eq(&b);\n}\n";
        let first = Position { file: "t.rs", line: 2, column: 5 };
        let second = Position { file: "t.rs", line: 5, column: 5 };
        let mut patchwork = Patchwork::new(file.to_owned());
        for (position, actual) in [(first, "one\n\"#two\n"), (second, "y")] {
            let loc = locate(file, position);
            patchwork.patch(loc.literal_range, &format_patch(Some(loc.line_indent), actual));
        }
        assert_eq!(
            patchwork.text,
            "fn t() {\n    expect![[r##\"\n        one\n        \"#two\n    \"##]].assert_eq(&a);\n    expect![\"y\"].assert_eq(&b);\n}\n"
        );
        let again = locate(&patchwork.text, first);
        let literal = &patchwork.text[again.literal_range];
        assert_eq!(literal_text(literal).map(trimmed).as_deref(), Some("one\n\"#two\n"));
    }
}
