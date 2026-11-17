//! Findings drawn on the code, as an editor's inline view draws them, in the terminal.
//!
//! ```text
//! error  this build of blaze cannot read the dependency `bstd` yet
//!   ╭─ Cargo.toml:7:1
//! 6 │ [dependencies]
//! 7 │ bstd = { package = "blaze-std", path = "/Volumes/Projects/blaze-macbuild-1…
//!   │ ╰─ nothing this package gets from it is checked
//!   ╰─ blaze explain dependency-not-read
//! ```
//!
//! One loud thing a finding: its span. Three levels of emphasis only: the span loud (bold, the
//! severity's underline); the severity word, the message and the label medium; all else quiet
//! (context, punctuation, the gutter, the rule, the closing line's facts). The severity's
//! colour (error red, warning yellow, note cyan) is in exactly three places: the word, the
//! underline, the label; no syntax role takes red, yellow or cyan. Code carries its structure
//! by brightness: a context line is dim, one tone; on the span's line names, keys and keywords
//! are the normal ink, punctuation and comments dim, literals one soft tint ([`tint`]).
//!
//! What is shown: the header line of the innermost item enclosing the span (a `fn`'s first
//! line, a TOML table's `[name]`), a skipped line between shown dim, more than one a `⋮`; the
//! span's line (a span over lines: its first and last); nothing below. The label hangs under
//! the span on a row of its own, `╰─ label` to the right or `label ─╯` to the left. A long line
//! is windowed around the span with `…`, tabs expanded, every width a display width, and no
//! row wider than [`Caps::width`]. Without colour or curly underlines the span is marked by a
//! row of `^` (a secondary span's by `-`). Findings close together in one file share a frame.
//! A fix is drawn under the code (removed text dim and struck through, inserted text green).
//!
//! [`Caps`] says what the terminal can do; [`detect`] builds it from what the caller read
//! (the variables, whether the stream is a terminal, its width, a `--color` choice) and
//! nothing here reads anything itself. Off a terminal the form is [`Form::Lines`]:
//! `path:line:col: level[rule]: message`, one line a finding and no escape at all.
//!
//! Source text and messages are data: a control character, an escape sequence, a byte that is
//! not UTF-8 or a bidirectional control in what is shown is drawn as a visible stand-in and
//! never reaches the terminal as itself.
//!
//! [`render`] is a pure function of its input and `Caps`: deterministic (rule 25), with every
//! allocation in the allocator passed in and no recursion.

mod draw;
pub(crate) mod lex;
pub mod plain;
#[cfg(test)]
mod plain_tests;
#[cfg(test)]
mod spec;
#[cfg(test)]
mod tests;
pub(crate) mod text;
pub mod words;

use blaze_own::{AString, AVec, Alloc};
use blaze_text::Writes;

use crate::Severity;
use text::{Ink, Pen, Plant, Style};

/// How many colours the terminal draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colour {
    /// None: no escape of any kind is written.
    None,
    Ansi16,
    Ansi256,
    /// 24-bit. The severity colours stay the terminal's own (its theme's red, yellow, cyan);
    /// only the literals' soft [`tint`] is a 24-bit colour.
    True,
}

/// The shape of the output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// One line a finding, `path:line:col: level[rule]: message`, with no escape: a pipe, a
    /// file, CI. Stable and greppable.
    Lines,
    /// The code frames, for a person at a terminal.
    Frames,
}

/// What the terminal can do: a value, built at the edge ([`detect`]) and passed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    pub form: Form,
    pub colour: Colour,
    /// SGR `4:3`: the span is underlined with a curl on the code itself.
    pub curly: bool,
    /// SGR `58`: the underline takes the level's colour.
    pub underline_colour: bool,
    /// OSC 8: the header's `path:line:col` opens the place.
    pub hyperlinks: bool,
    /// Box drawing, `…` and `⋮`; ASCII stand-ins when false.
    pub unicode: bool,
    /// The terminal's columns; at least [`MIN_WIDTH`] is used.
    pub width: u16,
    /// Whether the background is dark or light, as far as the environment says: it picks the
    /// literals' tint on a truecolor terminal ([`tint`]).
    pub background: Background,
}

/// The terminal's background, from `COLORFGBG` where a terminal sets it; never asked of the
/// terminal (an escape round trip costs latency at startup and can hang on a pipe).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Background {
    Unknown,
    Dark,
    Light,
}

/// The literals' tint on a dark background: grey-blue, 5.82:1 on #1e1e1e (WCAG), its
/// luminance 0.32 well below the ink's (#d4d4d4, 0.66).
pub const TINT_DARK: [u8; 3] = [0x8a, 0x9a, 0xb0];
/// On a light background: 5.44:1 on #ffffff, its luminance 0.14 well above black ink's 0.
pub const TINT_LIGHT: [u8; 3] = [0x5b, 0x6b, 0x80];
/// When the background is not known: one mid-tone, 4.49:1 on #1e1e1e and 3.71:1 on #ffffff.
pub const TINT_UNKNOWN: [u8; 3] = [0x76, 0x86, 0x9c];

/// The one tint of string and number literals on the span's line: a 24-bit grey-blue on a
/// truecolor terminal, by the background; `None` on 256 or 16 colours (literals are dim there).
pub fn tint(caps: Caps) -> Option<[u8; 3]> {
    match (caps.colour, caps.background) {
        (Colour::True, Background::Dark) => Some(TINT_DARK),
        (Colour::True, Background::Light) => Some(TINT_LIGHT),
        (Colour::True, Background::Unknown) => Some(TINT_UNKNOWN),
        _ => None,
    }
}

/// The fewest columns a frame is laid out in.
pub const MIN_WIDTH: u16 = 20;

impl Caps {
    /// A pipe, a file, CI.
    pub const LINES: Caps = Caps {
        form: Form::Lines,
        colour: Colour::None,
        curly: false,
        underline_colour: false,
        hyperlinks: false,
        unicode: true,
        width: 80,
        background: Background::Unknown,
    };

    /// Frames with no escape (`--color=never` at a terminal, `NO_COLOR`).
    pub const FRAMES: Caps = Caps { form: Form::Frames, ..Caps::LINES };

    /// Frames in the 16 colours, with `^` lines under the spans.
    pub const ANSI: Caps = Caps { colour: Colour::Ansi16, ..Caps::FRAMES };

    /// Frames with curly, coloured underlines and links: kitty, Ghostty, WezTerm, iTerm2.
    pub const RICH: Caps =
        Caps { colour: Colour::True, curly: true, underline_colour: true, hyperlinks: true, ..Caps::FRAMES };

    fn width(&self) -> u32 {
        u32::from(self.width.max(MIN_WIDTH))
    }
}

/// `--color`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Auto,
    Always,
    Never,
}

/// What a terminal can do, from what the caller read: `var` the environment's variables, whether
/// the stream is a terminal, its width (80 when unknown), and `--color`. Conventions: `--color`
/// first; under `auto`, `NO_COLOR` (set, not empty) turns colour off, `CLICOLOR_FORCE` (not
/// `0`) and a terminal turn it on, `TERM=dumb` and `CI` turn the frames off. Curly underlines,
/// their colour and links are drawn only by the terminals known to draw them (kitty, Ghostty,
/// WezTerm, iTerm2, foot, VS Code's, Alacritty for the underlines) and never inside tmux or
/// screen, which may not pass them on.
pub fn detect<'v>(var: &dyn Fn(&str) -> Option<&'v str>, terminal: bool, width: Option<u16>, choice: Choice) -> Caps {
    let set = |name: &str| var(name).is_some_and(|it| !it.is_empty());
    let term = var("TERM").unwrap_or("");
    let program = var("TERM_PROGRAM").unwrap_or("");
    let forced = var("CLICOLOR_FORCE").is_some_and(|it| !it.is_empty() && it != "0");
    let frames = match choice {
        Choice::Always => true,
        Choice::Never => terminal && term != "dumb" && !set("CI"),
        Choice::Auto => forced || (terminal && term != "dumb" && !set("CI")),
    };
    if !frames {
        return Caps::LINES;
    }
    let colour_on = match choice {
        Choice::Always => true,
        Choice::Never => false,
        Choice::Auto => forced || (!set("NO_COLOR") && term != "dumb"),
    };
    let colorterm = var("COLORTERM").unwrap_or("");
    let colour = match () {
        () if !colour_on => Colour::None,
        () if colorterm == "truecolor" || colorterm == "24bit" => Colour::True,
        () if term.contains("256color") => Colour::Ansi256,
        () => Colour::Ansi16,
    };
    let muxed = set("TMUX") || set("STY") || term.starts_with("screen") || term.starts_with("tmux");
    let kitty = term == "xterm-kitty" || set("KITTY_WINDOW_ID");
    let known = kitty
        || matches!(program, "ghostty" | "WezTerm" | "iTerm.app" | "vscode")
        || matches!(term, "xterm-ghostty" | "wezterm" | "foot" | "foot-extra");
    let underlines = colour != Colour::None && !muxed && (known || term == "alacritty");
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|name| var(name).filter(|it| !it.is_empty()));
    let unicode = known
        || locale.is_some_and(|it| {
            let it = it.as_bytes();
            it.windows(5).any(|w| w.eq_ignore_ascii_case(b"utf-8")) || it.windows(4).any(|w| w.eq_ignore_ascii_case(b"utf8"))
        });
    Caps {
        form: Form::Frames,
        colour,
        curly: underlines,
        underline_colour: underlines,
        hyperlinks: colour != Colour::None && !muxed && (known || set("WT_SESSION")),
        unicode,
        width: width.unwrap_or(80).max(MIN_WIDTH),
        background: background(var("COLORFGBG")),
    }
}

/// `COLORFGBG` (`fg;bg` or `fg;default;bg`, set by rxvt, Konsole and a few others): a
/// background colour of 0-6 or 8 is dark, any other number light, anything else unknown.
pub fn background(colorfgbg: Option<&str>) -> Background {
    let Some(last) = colorfgbg.and_then(|it| it.rsplit(';').next()) else { return Background::Unknown };
    match last.parse::<u8>() {
        Ok(0..=6 | 8) => Background::Dark,
        Ok(_) => Background::Light,
        Err(_) => Background::Unknown,
    }
}

/// A file a finding names: how it is shown (from the workspace), its absolute path (for a
/// link) and its bytes.
#[derive(Clone, Copy, Debug)]
pub struct Source<'a> {
    pub shown: &'a str,
    pub absolute: &'a str,
    pub text: &'a [u8],
}

/// A range of a file's bytes, `start..end` (file-relative: the printer adds the item's place
/// to a record's item-relative range), and what to say beside it. `start == end` marks the
/// point at `start`.
#[derive(Clone, Copy, Debug)]
pub struct Mark<'a> {
    pub start: u32,
    pub end: u32,
    /// Short; code in backticks. Empty for none.
    pub label: &'a str,
}

/// One edit of a fix: `start..end` of the file replaced by `insert`.
#[derive(Clone, Copy, Debug)]
pub struct Edit<'a> {
    pub start: u32,
    pub end: u32,
    pub insert: &'a str,
}

/// One finding to draw.
#[derive(Clone, Copy, Debug)]
pub struct Finding<'a> {
    pub severity: Severity,
    /// The rule's name, `area:name`, for `blaze explain`; empty for none.
    pub rule: &'a str,
    /// Plain words, code in backticks.
    pub message: &'a str,
    /// Its file, an index into [`Input::sources`].
    pub file: u32,
    /// The first byte of the item it is in, when known: where the search for the innermost
    /// item enclosing the span (whose header line the frame shows) may begin.
    pub item: Option<u32>,
    pub primary: Mark<'a>,
    /// The other places that matter (the earlier definition, the borrow still alive), in the
    /// same file, drawn in a quieter colour with their own labels.
    pub secondary: &'a [Mark<'a>],
    /// One fix's edits, sorted, not overlapping, in the same file; empty for none.
    pub fix: &'a [Edit<'a>],
}

/// What [`render`] draws.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    pub findings: &'a [Finding<'a>],
    pub sources: &'a [Source<'a>],
    /// The URL a header links to, `{path}` (absolute, percent-encoded), `{line}` and `{col}`
    /// filled in: [`LINK_FILE`], or an editor's form such as
    /// `vscode://file/{path}:{line}:{col}`.
    pub link: &'a str,
    /// Whether this command can apply the findings' fixes.
    pub fixes: Fixes,
    /// Each finding's id (D70), beside [`Input::findings`]; empty for none. Drawn only where a
    /// fix can be applied: `fix: blaze check --fix <id>`.
    pub ids: &'a [&'a str],
}

/// The default link: the file itself.
pub const LINK_FILE: &str = "file://{path}";

/// Why a finding was not drawn: a caller's mistake, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameError {
    /// Finding `finding` names file `file`, and there are only `sources`.
    NoSource { finding: usize, file: u32, sources: usize },
    /// A range of finding `finding` (`start..end`) is not within its file's `len` bytes, or
    /// ends before it starts.
    OutOfRange { finding: usize, start: u32, end: u32, len: usize },
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            FrameError::NoSource { finding, file, sources } => {
                write!(f, "blaze_report::frame: finding {finding} names file {file}, and {sources} were given")
            }
            FrameError::OutOfRange { finding, start, end, len } => write!(
                f,
                "blaze_report::frame: finding {finding} marks bytes {start}..{end} of a file of {len} bytes"
            ),
        }
    }
}

impl core::error::Error for FrameError {}

/// The counts of findings drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Tasks that failed on their own (a build script, a module lookup): each a cause printed
    /// once with what it stopped, never one line a dependent; not counted in `errors`, which are
    /// the code's.
    pub failures: u32,
    pub errors: u32,
    pub warnings: u32,
    pub notes: u32,
    /// The files the findings are in.
    pub files: u32,
    pub fixable: u32,
}

impl Counts {
    /// One more finding of `severity`, fixable or not (a command adding up findings it drew
    /// one at a time; it counts the files itself).
    pub fn add(&mut self, severity: Severity, fixable: bool) {
        match severity {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
            Severity::Note => self.notes += 1,
        }
        self.fixable += u32::from(fixable);
    }

    /// Of `findings`.
    pub fn of(findings: &[Finding<'_>]) -> Counts {
        let mut counts = Counts::default();
        for (at, finding) in findings.iter().enumerate() {
            match finding.severity {
                Severity::Error => counts.errors += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Note => counts.notes += 1,
            }
            counts.fixable += u32::from(!finding.fix.is_empty());
            counts.files += u32::from(!findings[..at].iter().any(|it| it.file == finding.file));
        }
        counts
    }
}

/// `input` drawn for `caps` at the end of `out`; scratch space from `alloc`.
pub fn render(out: &mut AString<'_>, input: &Input<'_>, caps: Caps, alloc: Alloc<'_>) -> Result<(), FrameError> {
    render_planted(out, input, caps, alloc, Plant::Clean)
}

pub(crate) fn render_planted(
    out: &mut AString<'_>,
    input: &Input<'_>,
    caps: Caps,
    alloc: Alloc<'_>,
    plant: Plant,
) -> Result<(), FrameError> {
    check(input)?;
    // Source order: by file, then where the primary span starts, then as given.
    let mut order: AVec<'_, u32> = AVec::new_in(alloc);
    for at in 0..input.findings.len() {
        order.push(at as u32);
    }
    order.sort_unstable_by_key(|&at| {
        let finding = &input.findings[at as usize];
        (finding.file, finding.primary.start, finding.primary.end, at)
    });
    let counts = Counts::of(input.findings);
    match caps.form {
        Form::Lines => {
            for &at in order.iter() {
                lines_into(out, input, &input.findings[at as usize]);
                // The id where a fix can be applied by it (D70), on its own line.
                if input.fixes == Fixes::Command
                    && !input.findings[at as usize].fix.is_empty()
                    && let Some(id) = input.ids.get(at as usize).filter(|id| !id.is_empty())
                {
                    out.str("  fix: blaze check --fix ").str(id).str("\n");
                }
            }
            return Ok(());
        }
        Form::Frames => draw::frames(out, input, &order, caps, counts, alloc, plant),
    }
    Ok(())
}

fn check(input: &Input<'_>) -> Result<(), FrameError> {
    for (at, finding) in input.findings.iter().enumerate() {
        let Some(source) = input.sources.get(finding.file as usize) else {
            return Err(FrameError::NoSource { finding: at, file: finding.file, sources: input.sources.len() });
        };
        let len = source.text.len();
        let ranges = core::iter::once((finding.primary.start, finding.primary.end))
            .chain(finding.secondary.iter().map(|it| (it.start, it.end)))
            .chain(finding.fix.iter().map(|it| (it.start, it.end)))
            .chain(finding.item.map(|it| (it, it)));
        for (start, end) in ranges {
            if start > end || end as usize > len {
                return Err(FrameError::OutOfRange { finding: at, start, end, len });
            }
        }
    }
    Ok(())
}

/// The 1-based line and column (in characters, a byte that is not UTF-8 one) of `byte`.
pub(crate) fn line_col(text: &[u8], byte: u32) -> (u32, u32) {
    let before = &text[..(byte as usize).min(text.len())];
    let line_start = before.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
    let line = before.iter().filter(|&&b| b == b'\n').count() as u32 + 1;
    let col = before[line_start..].iter().filter(|&&b| b & 0xc0 != 0x80).count() as u32 + 1;
    (line, col)
}

/// `path:line:col: level[rule]: message`, every control a stand-in.
fn lines_into(out: &mut AString<'_>, input: &Input<'_>, finding: &Finding<'_>) {
    let source = &input.sources[finding.file as usize];
    let (line, col) = line_col(source.text, finding.primary.start);
    let mut pen = Pen::new(out, Caps::LINES);
    pen.safe(Style::PLAIN, source.shown);
    pen.out.str(":").int(line).str(":").int(col).str(": ").str(finding.severity.label());
    if !finding.rule.is_empty() {
        pen.out.push_str("[");
        pen.safe(Style::PLAIN, finding.rule);
        pen.out.push_str("]");
    }
    pen.out.push_str(": ");
    pen.safe(Style::PLAIN, finding.message);
    pen.end();
}

pub(crate) fn ink(severity: Severity) -> Ink {
    match severity {
        Severity::Error => Ink::Error,
        Severity::Warning => Ink::Warning,
        Severity::Note => Ink::Note,
    }
}

/// What a command's one closing line says.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Closing<'a> {
    /// The stages this command is defined to run that it did not: `None` when it ran them all.
    /// Then the line never says `ok`: `incomplete · NOT checked: … · no errors in: …`.
    pub coverage: Option<Coverage<'a>>,
    /// The findings drawn.
    pub counts: Counts,
    /// The files the command checked.
    pub checked: u32,
    /// How long the command took, start to answer.
    pub nanos: u64,
    /// How many items were built again; `Some(0)` says `nothing changed`, `None` nothing.
    pub changed: Option<u32>,
    /// Whether this command can apply fixes: only then does the line count the fixable.
    pub fixes: Fixes,
    /// What `--fix` is given to apply them (D70): the one fix's id, or `all`; empty for none.
    pub fix_with: &'a str,
}

/// What an incomplete command checked and what it did not, each a list of stage words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Coverage<'a> {
    pub ran: &'a [&'a str],
    pub not: &'a [&'a str],
}

/// Whether the command drawing findings can apply their fixes, as the caller knows it: the
/// renderer never assumes a flag exists. A fix is drawn as a preview either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Fixes {
    /// No command applies them: the footer says `apply by hand`, the closing line claims none.
    #[default]
    ByHand,
    /// `blaze check --fix` applies them: the footer and the closing line say so.
    Command,
}

/// A command's last line, for a person: what happened and how long, quietly.
/// `1 error · 1 file checked · 1.1 ms`, or `ok · 14 files checked · 0.5 ms · nothing changed`;
/// `· 1 fixable: blaze check --fix` when a fix is offered and the command applies it. Parts that do not fit the width are
/// left off from the right. No escapes in the [`Form::Lines`] form. A `run` whose program ran
/// prints none: the program's output is the output.
pub fn closing(out: &mut AString<'_>, closing: &Closing<'_>, caps: Caps) {
    let caps = if caps.form == Form::Lines { Caps::LINES } else { caps };
    let mut pen = Pen::new(out, caps);
    let width = caps.width();
    let dot = if caps.unicode { " · " } else { " - " };
    let counts = closing.counts;
    let mut first = true;
    // Each part: its width, then drawn only when it fits.
    let mut part = |pen: &mut Pen<'_, '_>, width_of: u32, draw: &dyn Fn(&mut Pen<'_, '_>)| -> bool {
        let lead = if first { 0 } else { 3 };
        if pen.cols + lead + width_of > width {
            return false;
        }
        if !first {
            pen.put(Style::DIM, dot, 3);
        }
        first = false;
        draw(pen);
        true
    };
    let mut fits = true;
    for (count, one, many, ink) in [
        (counts.failures, "failure", "failures", Ink::Error),
        (counts.errors, "error", "errors", Ink::Error),
        (counts.warnings, "warning", "warnings", Ink::Warning),
        (counts.notes, "note", "notes", Ink::Note),
    ] {
        if count == 0 || !fits {
            continue;
        }
        let word = if count == 1 { one } else { many };
        fits = part(&mut pen, digits(count) + 1 + word.len() as u32, &|pen| {
            let style = Style { bold: true, ..Style { fg: ink.colour(), ..Style::PLAIN } };
            pen.style(style);
            pen.out.int(count).str(" ").str(word);
            pen.cols += digits(count) + 1 + word.len() as u32;
        });
    }
    // A stage missing: never `ok`, and each list said (the first word `incomplete`).
    let joined = |items: &[&str]| items.iter().map(|item| item.chars().count() as u32).sum::<u32>() + 2 * (items.len() as u32).saturating_sub(1);
    let list = |pen: &mut Pen<'_, '_>, lead: &str, items: &[&str]| {
        pen.put(Style::DIM, lead, lead.chars().count() as u32);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                pen.put(Style::DIM, ", ", 2);
            }
            pen.put(Style::DIM, item, item.chars().count() as u32);
        }
    };
    match closing.coverage {
        Some(coverage) => {
            // What was NOT checked comes first: a narrow line drops the tail, and the tail must
            // never be the stages a reader would take as passed.
            let clean = counts.failures + counts.errors + counts.warnings + counts.notes == 0;
            if clean {
                part(&mut pen, 10, &|pen| pen.put(Style { bold: true, ..Style { fg: Ink::Warning.colour(), ..Style::PLAIN } }, "incomplete", 10));
            }
            fits = fits && part(&mut pen, 13 + joined(coverage.not), &|pen| list(pen, "NOT checked: ", coverage.not));
            if clean && !coverage.ran.is_empty() {
                fits = fits && part(&mut pen, 14 + joined(coverage.ran), &|pen| list(pen, "no errors in: ", coverage.ran));
            }
        }
        None if counts.failures + counts.errors + counts.warnings + counts.notes == 0 => {
            part(&mut pen, 2, &|pen| pen.put(Style { bold: true, ..Style::fg(2) }, "ok", 2));
        }
        None => {}
    }
    // A fix is claimed only where this command can apply it.
    if counts.fixable > 0 && closing.fixes == Fixes::Command && fits {
        let with = closing.fix_with.len() as u32 + u32::from(!closing.fix_with.is_empty());
        fits = part(&mut pen, digits(counts.fixable) + 27 + with, &|pen| {
            pen.style(Style::DIM);
            pen.out.int(counts.fixable);
            pen.cols += digits(counts.fixable);
            pen.put(Style::DIM, " fixable: blaze check --fix", 27);
            if !closing.fix_with.is_empty() {
                pen.put(Style::DIM, " ", 1);
                pen.put(Style::DIM, closing.fix_with, with - 1);
            }
        });
    }
    let files = if closing.checked == 1 { " file checked" } else { " files checked" };
    fits = fits
        && part(&mut pen, digits(closing.checked) + files.len() as u32, &|pen| {
            pen.style(Style::DIM);
            pen.out.int(closing.checked).str(files);
            pen.cols += digits(closing.checked) + files.len() as u32;
        });
    let mut time = [0u8; 24];
    let time = duration(&mut time, closing.nanos);
    fits = fits && part(&mut pen, time.len() as u32, &|pen| pen.put(Style::DIM, time, time.len() as u32));
    if let Some(changed) = closing.changed
        && fits
    {
        match changed {
            0 => part(&mut pen, 15, &|pen| pen.put(Style::DIM, "nothing changed", 15)),
            n => part(&mut pen, digits(n) + 8, &|pen| {
                pen.style(Style::DIM);
                pen.out.int(n).str(" rebuilt");
                pen.cols += digits(n) + 8;
            }),
        };
    }
    pen.end();
}

/// A note about the command, not about a place in a file (an old home rebuilt, what this build
/// runs): one quiet line, `note: words`, the words dim. Drawn after the findings and before the
/// closing line. No escapes in the [`Form::Lines`] form.
pub fn note(out: &mut AString<'_>, words: &str, caps: Caps) {
    let caps = if caps.form == Form::Lines { Caps::LINES } else { caps };
    let mut pen = Pen::new(out, caps);
    pen.put(Style { bold: true, ..Style { fg: Ink::Note.colour(), ..Style::PLAIN } }, "note", 4);
    pen.put(Style::DIM, ": ", 2);
    pen.put(Style::DIM, words, words.chars().count() as u32);
    pen.end();
}

/// Why [`help`] drew nothing: the caller's fix does not fit its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelpError {
    /// The edit's range is not in the text, or is not on one line.
    NotOneLine { start: u32, end: u32, len: usize },
    /// The line is not UTF-8.
    NotText { line: u32 },
}

/// Advice attached to an error (the error stands either way): `help: words`, then, for a fix,
/// the line before it (`N - ..`) and after it (`N + ..`), whole. `text` is the file, `fix` one
/// edit on one line (file-relative: the printer adds the item's place to a record's range).
///
/// ```text
/// help: `b` is in scope, is a `u32`, and is never used
/// 1 - fn add(a: u32, b: u32) -> u32 { a + c }
/// 1 + fn add(a: u32, b: u32) -> u32 { a + b }
/// ```
pub fn help(out: &mut AString<'_>, words: &str, text: &[u8], fix: Option<Edit<'_>>, caps: Caps) -> Result<(), HelpError> {
    let caps = if caps.form == Form::Lines { Caps::LINES } else { caps };
    // The lines first, so nothing is drawn for a fix that does not fit.
    let mut lines = None;
    if let Some(e) = fix {
        let bad = HelpError::NotOneLine { start: e.start, end: e.end, len: text.len() };
        let (s, x) = (e.start as usize, e.end as usize);
        if s > x || x > text.len() || text[s..x].contains(&b'\n') {
            return Err(bad);
        }
        let first = text[..s].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let last = text[x..].iter().position(|&b| b == b'\n').map_or(text.len(), |i| x + i);
        let number = text[..s].iter().filter(|&&b| b == b'\n').count() as u32 + 1;
        let line = core::str::from_utf8(&text[first..last]).map_err(|_| HelpError::NotText { line: number })?;
        let (before, after) = (s - first, x - first);
        if !line.is_char_boundary(before) || !line.is_char_boundary(after) {
            return Err(HelpError::NotText { line: number });
        }
        lines = Some((number, line, before, after, e.insert));
    }
    let mut pen = Pen::new(out, caps);
    pen.put(Style { bold: true, ..Style { fg: Ink::Note.colour(), ..Style::PLAIN } }, "help", 4);
    pen.put(Style::PLAIN, ": ", 2);
    pen.put(Style::PLAIN, words, words.chars().count() as u32);
    pen.end();
    if let Some((number, line, before, after, insert)) = lines {
        let mut digits = [0u8; 10];
        let mut n = number;
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let number = core::str::from_utf8(&digits[at..]).unwrap_or("?");
        let width = |s: &str| s.chars().count() as u32;
        for (sign, ink, middle) in [("-", Ink::Error, &line[before..after]), ("+", Ink::Note, insert)] {
            pen.put(Style::DIM, number, width(number));
            pen.put(Style { fg: ink.colour(), ..Style::PLAIN }, " ", 1);
            pen.put(Style { fg: ink.colour(), ..Style::PLAIN }, sign, 1);
            pen.put(Style::PLAIN, " ", 1);
            pen.put(Style::PLAIN, &line[..before], width(&line[..before]));
            pen.put(Style { fg: ink.colour(), ..Style::PLAIN }, middle, width(middle));
            pen.put(Style::PLAIN, &line[after..], width(&line[after..]));
            pen.end();
        }
    }
    Ok(())
}

/// `0.5 ms`, `1.1 ms`, `312 ms`, `2.4 s`, `95 s`: two or three figures.
fn duration(buf: &mut [u8; 24], nanos: u64) -> &str {
    let (tenths, unit) = match nanos {
        ..100_000_000 => ((nanos + 50_000) / 100_000, " ms"),
        100_000_000..1_000_000_000 => ((nanos + 500_000) / 1_000_000 * 10, " ms"),
        _ => ((nanos + 50_000_000) / 100_000_000, " s"),
    };
    let whole = tenths / 10;
    let mut len = 0;
    let mut put = |byte: u8| {
        buf[len] = byte;
        len += 1;
    };
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut n = whole;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for at in (0..count).rev() {
        put(digits[at]);
    }
    if whole < 10 {
        put(b'.');
        put(b'0' + (tenths % 10) as u8);
    }
    for &byte in unit.as_bytes() {
        put(byte);
    }
    core::str::from_utf8(&buf[..len]).unwrap_or("")
}

pub(crate) fn digits(n: u32) -> u32 {
    n.checked_ilog10().unwrap_or(0) + 1
}
