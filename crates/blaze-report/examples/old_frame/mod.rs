//! THE OLD RENDERER, as it stood on 2026-11-16 before the redesign (src/frame), kept only so
//! the demo can show before and after side by side. Not built into the crate.
//!
//! Findings drawn on the code, as an editor's inline view draws them, in the terminal.
//!
//! ```text
//! error  the `?` operator is not supported by this build of blaze yet
//!   ╭─ src/main.rs:2:18 · fn main
//! 1 │ fn main(cx: &Cx, io: Io) -> Result<(), Error> {
//! 2 │     io.need_out()?.line(cx, "Hello, world!")?;   `?` here
//! 3 │     Ok(())
//!   ╰─ blaze explain ladder:unsupported
//! ```
//!
//! For each finding: a headline (the level's word, the message with its code set apart), then
//! the code, highlighted from blaze-lex's tokens in the terminal's own 16 colours, with the
//! span marked on the code itself: a curly underline in the level's colour where the terminal
//! draws one (SGR `4:3` with an SGR `58` colour) and its label dimmed at the line's end, else a
//! line of `^` (and `-` for a secondary span) under it carrying the label. Never both.
//!
//! What is shown is chosen: the span's lines and one each side, the enclosing item's header
//! line with `⋮` when the span is far below it, a long line windowed around the span with `…`,
//! tabs expanded, every width a display width, and no line wider than [`Caps::width`].
//! Findings close together in one file share one frame. A fix is drawn as the changed line
//! under the code (removed text struck through, inserted text green), and the frame's last
//! line names how to apply it. One quiet summary line ends the output.
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
pub(crate) mod text;

use blaze_own::{AString, AVec, Alloc};
use blaze_text::Writes;

use blaze_report::Severity;
use text::{Ink, Pen, Plant, Style};

/// How many colours the terminal draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colour {
    /// None: no escape of any kind is written.
    None,
    Ansi16,
    Ansi256,
    /// 24-bit. The frame still uses the terminal's own 16 colours, so the code reads in the
    /// user's theme; a truecolor theme is an option to add, not the default.
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
    /// The first byte of the item it is in: the frame shows that item's header line.
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
pub struct Closing {
    /// The findings drawn.
    pub counts: Counts,
    /// The files the command checked.
    pub checked: u32,
    /// How long the command took, start to answer.
    pub nanos: u64,
    /// How many items were built again; `Some(0)` says `nothing changed`, `None` nothing.
    pub changed: Option<u32>,
}

/// A command's last line, for a person: what happened and how long, quietly.
/// `1 error · 1 file checked · 1.1 ms`, or `ok · 14 files checked · 0.5 ms · nothing changed`;
/// `· 1 fixable: blaze check --fix` when a fix is offered. Parts that do not fit the width are
/// left off from the right. No escapes in the [`Form::Lines`] form. A `run` whose program ran
/// prints none: the program's output is the output.
pub fn closing(out: &mut AString<'_>, closing: &Closing, caps: Caps) {
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
    if counts.errors + counts.warnings + counts.notes == 0 {
        part(&mut pen, 2, &|pen| pen.put(Style { bold: true, ..Style::fg(2) }, "ok", 2));
    }
    if counts.fixable > 0 && fits {
        fits = part(&mut pen, digits(counts.fixable) + 27, &|pen| {
            pen.style(Style::PLAIN);
            pen.out.int(counts.fixable);
            pen.cols += digits(counts.fixable);
            pen.put(Style::DIM, " fixable: ", 10);
            pen.put(Style::PLAIN, "blaze check --fix", 17);
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
