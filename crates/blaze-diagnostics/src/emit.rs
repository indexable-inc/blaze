//! Diagnostics written as rustc writes them: as text (`--error-format=human` and `short`, in
//! colour or not), as `--error-format=json`, and the closing ones after a crate's.
//!
//! Follows rustc's `compiler/rustc_errors/src/annotate_snippet_emitter_writer.rs`, `emitter.rs`
//! (`primary_span_formatted`, `fix_multispans_in_extern_macros_and_render_macro_backtrace`,
//! `is_different`, `detect_confusion_type`, `normalize_whitespace`), `json.rs` and `lib.rs`
//! (`DiagCtxtInner::print_error_count`, `emit_to_destination`), and `compiler/rustc_span/src/
//! lib.rs` (`normalize_src`, `analyze_source_file`, `original_relative_byte_pos`,
//! `macro_backtrace`) at rust-lang/rust ad3d0bc141a02cf446e384136d250a1f6950fed5 (MIT or
//! Apache-2.0), with the version of `annotate-snippets` rustc lays its text out with, and
//! `Renderer::styled()`'s colours as rustc takes them. Generalized from `blaze-lints`' emitter
//! of clippy's lints to every diagnostic rustc gives.
//!
//! Each file is read and indexed once, into an arena of [`Sources`] by `u32`; each diagnostic
//! looks its files up once; what is rendered borrows the text of the files and of the
//! diagnostic, and copies none of it. The bar is rustc's own: its `analyze_source_file` finds
//! lines 16 bytes at a time with SSE2, where `span::LineIndex` makes its newline bitmap 64 bytes
//! at a time with AVX-512BW, AVX2, SSE2 or NEON, picked at run time, and
//! the layout is the same `annotate-snippets` rustc renders with, given borrowed text where
//! rustc formats each line into a `String` first.

pub mod codes;
mod json;
#[cfg(test)]
mod reference_tests;

use std::{borrow::Cow, fmt};

use annotate_snippets::{AnnotationKind, Group, Patch, Renderer, Snippet};
use blaze_hash::FixedState;
use blaze_ids::radix;
use smallvec::{SmallVec, smallvec};

use crate::model::{
    Code, Diag, Level, MacroKind, MultiSpan, Span, Sublevel, Suggestion, SuggestionStyle,
};

/// rustc's `MAX_SUGGESTIONS`: the most ways to substitute that one suggestion shows.
const MAX_SUGGESTIONS: usize = 4;

/// The width rustc lays diagnostics out for when it is not told one, as under cargo.
pub const WIDTH: usize = annotate_snippets::renderer::DEFAULT_TERM_WIDTH;

/// The index of a file that can't be read: past the end of every arena of files.
const UNREAD: u32 = u32::MAX;

/// What ends the shown name of a file with no path (a crate attribute's) in the name its spans
/// give it, the file's text after it. rustc's `FileName::CliCrateAttr` is likewise the name and
/// a hash of the text, shown as the name alone: files shown by one name stay apart, and no
/// caller reads the text. No path and no name rustc shows holds a NUL.
const TEXT_AFTER: char = '\0';

/// The name the spans of a file with no path give it: `shown`, then the file's `text`.
pub fn name_with_text(shown: &str, text: &str) -> String {
    // alloc: a span's file name, made only as a diagnostic is built
    let mut name = String::with_capacity(shown.len() + TEXT_AFTER.len_utf8() + text.len());
    name.push_str(shown);
    name.push(TEXT_AFTER);
    name.push_str(text);
    name
}

/// The name rustc shows the file `name` by: without the text a file with no path carries.
pub fn shown(name: &str) -> &str {
    name.split_once(TEXT_AFTER).map_or(name, |(shown, _)| shown)
}

/// The text of the file `name`, when it is one with no path, which carries it.
fn text_in(name: &str) -> Option<&str> {
    name.split_once(TEXT_AFTER).map(|(_, text)| text)
}

/// The text of the files diagnostics point into, by the name they give them. The caller reads
/// them: [`Sources::unread`] names the files a batch of diagnostics needs, each given once to
/// [`Sources::add`] before the batch is written.
#[derive(Default)]
pub struct Sources {
    /// Each file's index in `files` by its name, or [`UNREAD`]: one hash probe per lookup.
    names: hashbrown::HashMap<String, u32, FixedState>,
    files: Vec<File>,
}

/// A span a diagnostic can't be written with. rustc always has the files its spans point into,
/// so one is a bug of whoever made the span or gathered the files' text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unplaced {
    /// No text of the file was given to [`Sources::add`], or none could be read.
    Unread { file: String, byte: u32 },
    /// The file's text ends before the span does.
    PastEnd { file: String, byte: u32, len: u32 },
}

impl fmt::Display for Unplaced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unplaced::Unread { file, byte } => {
                write!(f, "a diagnostic's span at byte {byte} of `{file}`, whose text was not read")
            }
            Unplaced::PastEnd { file, byte, len } => {
                write!(f, "a diagnostic's span at byte {byte} of `{file}`, which is {len} bytes long")
            }
        }
    }
}

impl std::error::Error for Unplaced {}

/// A file as rustc's `SourceFile` holds it: its text without a byte order mark and with each
/// `\r\n` as `\n` (`normalize_src`), where its lines start, and where what was taken out was.
/// Spans are in bytes of the file as read; the text is shown and measured as rustc has it.
///
/// The text is the `String` read, not a copy, unless a BOM or a `\r\n` is taken out. An eighth
/// of a byte per byte of text for its lines ([`span::LineIndex`]) and 8 bytes a `\r\n`; placing
/// a byte is a rank of the newline bitmap, then a count of
/// the line's chars only in a file that is not ASCII (rustc keeps its multibyte chars for it).
struct File {
    text: String,
    lines: span::LineIndex,
    /// Each `\r` taken out: where it was in the file as read, and where its `\n` is in `text`.
    crs: Vec<(u32, u32)>,
    /// The length of the file as read.
    len: u32,
    /// The length of the byte order mark taken out, if there was one.
    bom: u32,
    ascii: bool,
    /// Whether rustc shows its source (`should_show_source_code`): false for a file of another
    /// crate whose source is not present, whose spans rustc places but neither shows, quotes
    /// nor splices into.
    shown: bool,
}

impl File {
    fn new(read: String) -> File {
        let len = read.len() as u32;
        let bom = if read.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let mut crs = Vec::new(); // alloc: only for a file with `\r\n`, placed for a diagnostic written
        let text = match bom == 0 && memchr::memmem::find(read.as_bytes(), b"\r\n").is_none() {
            true => read,
            false => {
                let mut text = String::with_capacity(read.len() - bom);
                let mut from = bom;
                for at in memchr::memmem::find_iter(read.as_bytes(), b"\r\n") {
                    text.push_str(&read[from..at]);
                    crs.push((at as u32, text.len() as u32));
                    from = at + 1;
                }
                text.push_str(&read[from..]);
                text
            }
        };
        let lines = span::LineIndex::new(&text);
        let ascii = text.is_ascii();
        File { text, lines, crs, len, bom: bom as u32, ascii, shown: true }
    }

    /// Where byte `read` of the file as read is in `text`.
    fn norm(&self, read: u32) -> usize {
        let read = read.min(self.len);
        let taken = self.crs.partition_point(|(cr, _)| *cr < read) as u32;
        self.text.floor_char_boundary((read.saturating_sub(self.bom) - taken) as usize)
    }

    /// rustc's `original_relative_byte_pos`: the byte of the file as read that byte `at` of
    /// `text` is.
    fn original(&self, at: usize) -> u32 {
        let taken = self.crs.partition_point(|(_, lf)| (*lf as usize) < at) as u32;
        at as u32 + self.bom + taken
    }

    /// Whether `span` is in the file: rustc's `is_valid_span`.
    fn holds(&self, span: &Span) -> bool {
        span.lo <= span.hi && span.hi <= self.len
    }

    /// The 0-based line byte `at` of `text` is on.
    fn line_of(&self, at: usize) -> usize {
        let at = span::TextSize::new(at.min(self.text.len()) as u32);
        self.lines.line(at).expect("an offset up to the text's end") as usize
    }

    /// The bytes of 0-based line `line`, from its start to the start of the next one.
    fn line_bounds(&self, line: usize) -> std::ops::Range<usize> {
        let start_of = |line: usize| {
            u32::try_from(line).ok().and_then(|line| self.lines.line_start(line)).map_or(self.text.len(), usize::from)
        };
        start_of(line)..start_of(line + 1)
    }

    /// The text of 0-based line `line`, without its `\n`: rustc's `get_line`.
    fn line_text(&self, line: usize) -> &str {
        let text = &self.text[self.line_bounds(line)];
        text.strip_suffix('\n').unwrap_or(text)
    }

    /// The 1-based line and the 0-based column in chars of byte `at` of `text`.
    fn position(&self, at: usize) -> (usize, usize) {
        let line = self.line_of(at);
        let start = self.line_bounds(line).start;
        let column = match self.ascii {
            true => at - start,
            false => self.text[start..at].chars().count(),
        };
        (line + 1, column)
    }

    /// The text of `lo..hi` of `text`.
    fn slice(&self, lo: usize, hi: usize) -> Option<&str> {
        self.text.get(lo..hi)
    }

    /// The text of `span` as rustc has it, when the file holds it and rustc has its source:
    /// `span_to_snippet`.
    fn snippet(&self, span: &Span) -> Option<&str> {
        match self.shown && self.holds(span) {
            true => self.slice(self.norm(span.lo), self.norm(span.hi)),
            false => None,
        }
    }
}

impl Sources {
    /// Files whose text is given.
    pub fn of(files: impl IntoIterator<Item = (String, String)>) -> Sources {
        let mut sources = Sources::default();
        for (name, text) in files {
            sources.add(&name, Some(text));
        }
        sources
    }

    /// The files `diags` point into, through each macro call and definition their spans are
    /// in, that were not given to [`Sources::add`]: each once, in the order first pointed into.
    /// A file with no path is not one: its name carries its text ([`name_with_text`]).
    pub fn unread<'d>(&self, diags: impl IntoIterator<Item = &'d Diag>) -> Vec<&'d str> {
        let mut seen: hashbrown::HashSet<&'d str, FixedState> = hashbrown::HashSet::default();
        let mut unread = Vec::new(); // alloc: the files diagnostics written point into, the edge
        for diag in diags {
            each_file(diag, &mut |name| {
                if text_in(name).is_none() && !self.names.contains_key(name) && seen.insert(name) {
                    unread.push(name);
                }
            });
        }
        unread
    }

    /// The text of the file `name`, as the caller read it; `None` for one it can't read, whose
    /// spans are then a bug of whoever made them. A name given twice keeps its first text.
    pub fn add(&mut self, name: &str, text: Option<String>) {
        let Sources { names, files } = self;
        names.entry_ref(name).or_insert_with(|| match text {
            Some(text) => {
                files.push(File::new(text));
                (files.len() - 1) as u32
            }
            None => UNREAD,
        });
    }

    /// The text of the file `name`, which rustc places spans in but whose source it doesn't
    /// have to show: a file of another crate when `ensure_source_file_source_present` fails, as
    /// core's does under compiletest. Its spans are written as rustc's `unannotated_messages`,
    /// where they are with no snippet. A name given twice keeps its first text.
    pub fn add_unshown(&mut self, name: &str, text: String) {
        let Sources { names, files } = self;
        names.entry_ref(name).or_insert_with(|| {
            files.push(File { shown: false, ..File::new(text) });
            (files.len() - 1) as u32
        });
    }

    /// Whether every span `diag` writes is in a file given here, and inside its text: what
    /// [`written`], [`rendered`] and [`crate::api::diagnostic`] need, checked before they
    /// place any.
    pub fn check(&self, diag: &Diag) -> Result<(), Unplaced> {
        let mut unplaced = None;
        each_span(diag, &mut |span| {
            if unplaced.is_none() {
                unplaced = [span.lo, span.hi].into_iter().find_map(|byte| self.unplaced(&span.file, byte));
            }
        });
        unplaced.map_or(Ok(()), Err)
    }

    /// Why byte `at` of `name` can't be placed, if it can't: [`Sources::placed`]'s refusals,
    /// without placing it or indexing a carried text.
    fn unplaced(&self, name: &str, at: u32) -> Option<Unplaced> {
        let len = match text_in(name) {
            Some(text) => text.len() as u32,
            None => match self.files.get(self.index(name) as usize) {
                Some(it) => it.len,
                // alloc: an error's file name, once per diagnostic that can't be written
                None => return Some(Unplaced::Unread { file: shown(name).to_owned(), byte: at }),
            },
        };
        // alloc: as above
        (at > len).then(|| Unplaced::PastEnd { file: shown(name).to_owned(), byte: at, len })
    }

    /// The 1-based line and 1-based column (in chars) of byte `at` of `file`.
    pub fn position(&self, file: &str, at: u32) -> Option<(u32, u32)> {
        self.placed(file, at).ok()
    }

    /// [`Sources::position`], or why byte `at` of `name` can't be placed.
    pub fn placed(&self, name: &str, at: u32) -> Result<(u32, u32), Unplaced> {
        let carried;
        let file = match text_in(name) {
            Some(text) => {
                // alloc: a file with no path, indexed for a span of it written
                carried = File::new(text.to_owned());
                &carried
            }
            None => match self.files.get(self.index(name) as usize) {
                Some(it) => it,
                // alloc: an error's file name, once per diagnostic that can't be written
                None => return Err(Unplaced::Unread { file: shown(name).to_owned(), byte: at }),
            },
        };
        if at > file.len {
            // alloc: as above
            return Err(Unplaced::PastEnd { file: shown(name).to_owned(), byte: at, len: file.len });
        }
        let (line, column) = file.position(file.norm(at));
        Ok((line as u32, column as u32 + 1))
    }

    /// The index of the file `name` in `files`, or [`UNREAD`].
    fn index(&self, name: &str) -> u32 {
        self.names.get(name).copied().unwrap_or(UNREAD)
    }
}

/// The files with no path one diagnostic points into, each indexed from the text its name
/// carries, in the order [`each_file`] first meets them: the diagnostic's own, as [`Sources`]
/// holds the files read. None for most diagnostics, which allocates nothing.
struct Carried(SmallVec<[File; 1]>);

impl Carried {
    fn of(diag: &Diag) -> Carried {
        let mut names: SmallVec<[&str; 1]> = SmallVec::new();
        let mut files: SmallVec<[File; 1]> = SmallVec::new();
        each_file(diag, &mut |name| {
            if let Some(text) = text_in(name)
                && !names.contains(&name)
            {
                names.push(name);
                // alloc: a file with no path, indexed for a diagnostic written
                files.push(File::new(text.to_owned()));
            }
        });
        Carried(files)
    }
}

/// The files one diagnostic points into, each looked up in [`Sources`] once, or in the
/// diagnostic's [`Carried`] for one with no path. A diagnostic has one or two, so a scan
/// comparing names costs less than hashing them again.
struct Placed<'a> {
    names: SmallVec<[(&'a str, Held); 4]>,
    files: &'a [File],
    carried: &'a [File],
}

/// Where [`Placed`] has a file: by its index in [`Sources`] or in [`Carried`].
#[derive(Clone, Copy)]
enum Held {
    Read(u32),
    Carried(u32),
}

impl<'a> Placed<'a> {
    fn new(diag: &'a Diag, sources: &'a Sources, carried: &'a Carried) -> Placed<'a> {
        let mut names: SmallVec<[(&'a str, Held); 4]> = SmallVec::new();
        let mut next = 0;
        each_file(diag, &mut |name| {
            if names.iter().any(|(it, _)| *it == name) {
                return;
            }
            let held = match text_in(name) {
                Some(_) => {
                    next += 1;
                    Held::Carried(next - 1)
                }
                None => Held::Read(sources.index(name)),
            };
            names.push((name, held));
        });
        Placed { names, files: &sources.files, carried: &carried.0 }
    }

    fn get(&self, name: &str) -> Option<&'a File> {
        let &(_, held) = self.names.iter().find(|(it, _)| *it == name)?;
        match held {
            Held::Read(index) => self.files.get(index as usize),
            Held::Carried(index) => self.carried.get(index as usize),
        }
    }

    /// The file `span` is in.
    fn file_of(&self, span: &Span) -> &'a File {
        self.file_named(&span.file, span.lo, span.hi)
    }

    /// [`Placed::file_of`] a span at `lo..hi` of `name`. Every writer holds its diagnostic to
    /// [`Sources::check`] before it places one, so each span's file is here.
    fn file_named(&self, name: &str, lo: u32, hi: u32) -> &'a File {
        self.get(name).unwrap_or_else(|| {
            panic!("blaze-diagnostics: a span at bytes {lo}..{hi} of `{name}` passed `Sources::check` and has no file")
        })
    }
}

fn spans_of(span: &MultiSpan) -> impl Iterator<Item = &Span> {
    span.primary.iter().chain(span.labels.iter().map(|(it, _)| it))
}

/// Each file `diag` points into: of its spans, its children's and its suggestions', and of
/// each macro call and definition they are in. A file may be met more than once.
fn each_file<'a>(diag: &'a Diag, f: &mut impl FnMut(&'a str)) {
    each_span(diag, &mut |span| f(&span.file));
}

/// Each span `diag` writes: its own, its children's and its suggestions', and each macro call
/// and definition they are in.
fn each_span<'a>(diag: &'a Diag, f: &mut impl FnMut(&'a Span)) {
    let spans = spans_of(&diag.span)
        .chain(diag.children.iter().flat_map(|it| spans_of(&it.span)))
        .chain(diag.suggestions.iter().flat_map(|it| it.substitutions.iter().flatten().map(|(span, _)| span)));
    for span in spans {
        meet(span, f);
    }
}

/// `span` and each macro call and definition it is in.
fn meet<'a>(mut span: &'a Span, f: &mut impl FnMut(&'a Span)) {
    loop {
        f(span);
        let Some(expansion) = span.expansion.as_deref() else { return };
        if let Some(def) = &expansion.def_site {
            meet(def, f);
        }
        span = &expansion.call_site;
    }
}

/// How the text of a diagnostic is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// `--error-format=short` (`--json=diagnostic-short`): a line of where it is and what it
    /// says.
    pub short: bool,
    /// In colour, as to a terminal (`--color=always`, `--json=diagnostic-rendered-ansi`).
    pub ansi: bool,
    /// The columns it is laid out in (`--diagnostic-width`): rustc takes 140 when not told.
    pub width: usize,
    /// `-Z ui-testing`, as rustc's UI tests' `.stderr` files have it: line numbers in the
    /// margin as `LL`.
    pub ui_testing: bool,
}

impl Default for Style {
    fn default() -> Style {
        Style { short: false, ansi: false, width: WIDTH, ui_testing: false }
    }
}

/// `diag` as rustc writes it, without the empty line it writes after one. Each of these writers
/// first holds `diag` to [`Sources::check`]: a span `sources` can't place is an error naming
/// its file, and none is placed after it.
pub fn rendered(diag: &Diag, sources: &Sources, style: Style) -> Result<String, Unplaced> {
    sources.check(diag)?;
    Ok(render(diag, &Placed::new(diag, sources, &Carried::of(diag)), style))
}

/// `rendered` as rustc writes it to stderr: with the empty line after it (none after short, nor
/// after a failure note). Under `short` rustc names no error codes to explain (its emitter
/// can't show them), so one of [`summary`]'s notes naming them is the empty string.
pub fn written(diag: &Diag, sources: &Sources, style: Style) -> Result<String, Unplaced> {
    if style.short && explains(diag) {
        return Ok(String::new()); // alloc: empty, allocates nothing
    }
    sources.check(diag)?;
    Ok(write(diag, &Placed::new(diag, sources, &Carried::of(diag)), style))
}

/// rustc's `--error-format=json` line of `diag` (keys in rustc's order), without the newline
/// after it, its `rendered` as `style` says (`--json=diagnostic-short`,
/// `--json=diagnostic-rendered-ansi`). Under `short`, one of [`summary`]'s notes naming error
/// codes to explain is the empty string, as in [`written`].
pub fn json(diag: &Diag, sources: &Sources, style: Style) -> Result<String, Unplaced> {
    if style.short && explains(diag) {
        return Ok(String::new()); // alloc: empty, allocates nothing
    }
    sources.check(diag)?;
    let carried = Carried::of(diag);
    let placed = Placed::new(diag, sources, &carried);
    let rendered = write(diag, &placed, style);
    Ok(json::line(diag, &placed, rendered))
}

/// [`json`] with `rendered` as its text: [`written`] of `diag` in the same style, made
/// already.
pub fn json_rendered(diag: &Diag, sources: &Sources, rendered: String) -> Result<String, Unplaced> {
    sources.check(diag)?;
    let carried = Carried::of(diag);
    let placed = Placed::new(diag, sources, &carried);
    Ok(json::line(diag, &placed, rendered))
}

fn write<'a>(diag: &'a Diag, placed: &Placed<'a>, style: Style) -> String {
    let mut text = render(diag, placed, style);
    text.push('\n');
    if !style.short && diag.level != Level::FailureNote {
        text.push('\n');
    }
    text
}

/// Whether `diag` is one of the closing notes naming error codes to explain, which rustc's
/// `print_error_count` leaves out when its emitter can't show them (`should_show_explain`).
fn explains(diag: &Diag) -> bool {
    diag.level == Level::FailureNote
        && diag.code.is_none()
        && (diag.message.starts_with("Some errors have detailed explanations: ")
            || diag.message.starts_with("For more information about this error, try `rustc --explain ")
            || diag.message.starts_with("For more information about an error, try `rustc --explain "))
}

/// rustc's closing diagnostics after a crate's: "aborting due to N previous errors; M warnings
/// emitted", "N warnings emitted", "Some errors have detailed explanations: ...", "For more
/// information about this error, try `rustc --explain E0308`." (failure-note), as rustc's
/// `DiagCtxtInner::print_error_count` gives them.
pub fn summary<'d>(diags: impl IntoIterator<Item = &'d Diag> + Clone) -> Vec<Diag> {
    let (mut errors, mut warnings) = (0usize, 0usize);
    // clone: the iterator, read twice; the diagnostics are borrowed
    for diag in diags.clone() {
        errors += usize::from(diag.level == Level::Error);
        warnings += usize::from(diag.level == Level::Warning);
    }
    if errors == 0 && warnings == 0 {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let warned = match warnings {
        0 => String::new(), // alloc: empty, allocates nothing
        1 => "1 warning emitted".to_owned(), // alloc: a closing line written, the edge
        count => format!("{count} warnings emitted"), // alloc: a closing line written, the edge
    };
    let aborted = match errors {
        0 => String::new(), // alloc: empty, allocates nothing
        1 => "aborting due to 1 previous error".to_owned(), // alloc: a closing line written, the edge
        count => format!("aborting due to {count} previous errors"), // alloc: a closing line written, the edge
    };
    let mut closing = Vec::with_capacity(3);
    match (errors, warnings) {
        (0, _) => closing.push(Diag::new(Level::Warning, None, warned)),
        (_, 0) => closing.push(Diag::new(Level::Error, None, aborted)),
        // alloc: a closing line written, the edge
        (_, _) => closing.push(Diag::new(Level::Error, None, format!("{aborted}; {warned}"))),
    }
    let mut explained: Vec<&str> = diags
        .into_iter()
        .filter_map(|it| it.code.as_ref().and_then(Code::error))
        .filter(|code| codes::explanation(code).is_some())
        .collect(); // alloc: the codes named in the closing lines, the edge
    explained.sort_unstable();
    explained.dedup();
    match &explained[..] {
        [] => {}
        [code] => closing.push(Diag::new(
            Level::FailureNote,
            None,
            // alloc: a closing line written, the edge
            format!("For more information about this error, try `rustc --explain {code}`."),
        )),
        [first, ..] => {
            let shown = explained.len().min(9);
            let more = if explained.len() > 9 { "..." } else { "." };
            closing.push(Diag::new(
                Level::FailureNote,
                None,
                // alloc: a closing line written, the edge
                format!(
                    "Some errors have detailed explanations: {}{more}",
                    explained[..shown].join(", ")
                ),
            ));
            closing.push(Diag::new(
                Level::FailureNote,
                None,
                // alloc: a closing line written, the edge
                format!("For more information about an error, try `rustc --explain {first}`."),
            ));
        }
    }
    closing
}

/// Labels the emitter adds to a span: the short suggestion shown inline and the macro calls.
type Labels<'a> = SmallVec<[(&'a Span, Cow<'a, str>); 2]>;

/// rustc's `AnnotateSnippetEmitter::emit_diagnostic` and `emit_messages_default`.
fn render<'a>(diag: &'a Diag, placed: &Placed<'a>, style: Style) -> String {
    let mut suggestions: &'a [Suggestion] = match diag.suggestions_disabled {
        true => &[],
        false => &diag.suggestions,
    };
    let mut extra: Labels<'a> = SmallVec::new();
    if let Some(label) = primary_span_formatted(suggestions, placed) {
        extra.push(label);
        suggestions = &[];
    }
    macro_labels(&diag.span, &mut extra);

    let renderer = match style.ansi {
        true => Renderer::styled(),
        false => Renderer::plain(),
    }
    .term_width(style.width)
    .anonymized_line_numbers(style.ui_testing)
    .decor_style(annotate_snippets::renderer::DecorStyle::Ascii)
    .short_message(style.short);

    let mut title = level_of(diag.level).primary_title(diag.message.as_str());
    if let Some(code) = diag.code.as_ref().and_then(Code::error) {
        title = title.id(code);
    }
    let mut report: Vec<Group<'a>> = Vec::with_capacity(2 + diag.children.len() + suggestions.len());
    let mut group = Group::with_title(title);

    if diag.span.primary_span().is_some() {
        let unshown = Unshown {
            level: level_of(diag.level),
            short: style.short,
            pad_last: diag.children.first().is_some_and(|it| it.span.is_empty()),
        };
        group = snippets(group, &diag.span, extra, placed, &mut report, unshown);
    }

    for child in &diag.children {
        let level = sublevel_of(child.level);
        let message = ui_dated(child.message.as_str(), style.ui_testing);
        if child.span.is_empty() {
            group = group.element(level.message(message));
            continue;
        }
        report.push(std::mem::replace(&mut group, Group::with_title(level.secondary_title(message))));
        if child.span.primary_span().is_some() {
            let mut extra: Labels<'a> = SmallVec::new();
            macro_labels(&child.span, &mut extra);
            let unshown = Unshown { level: sublevel_of(child.level), short: style.short, pad_last: false };
            group = snippets(group, &child.span, extra, placed, &mut report, unshown);
        }
    }
    if let Some(note) = diag.origin_note()
        && !diag.children.iter().any(|it| it.message == note)
    {
        group = group.element(annotate_snippets::Level::NOTE.message(note));
    }

    for suggestion in suggestions {
        match suggestion.style {
            SuggestionStyle::CompletelyHidden => {}
            SuggestionStyle::HideCodeAlways => {
                group = group
                    .element(annotate_snippets::Level::HELP.message(suggestion.message.as_str()));
            }
            SuggestionStyle::HideCodeInline
            | SuggestionStyle::ShowCode
            | SuggestionStyle::ShowAlways => {
                let substitutions = substitutions(&suggestion.substitutions, placed);
                let Some(&(name, ..)) = substitutions.first() else { continue };
                let mut message = Cow::Borrowed(suggestion.message.as_str());
                let others = substitutions.len().saturating_sub(MAX_SUGGESTIONS);
                let mut subs = Vec::with_capacity(substitutions.len().min(MAX_SUGGESTIONS));
                for (_, file, mut parts) in substitutions.into_iter().take(MAX_SUGGESTIONS) {
                    let confusion = parts.iter().fold(Confusion::None, |it, part| {
                        it.combine(detect_confusion_type(file, &part.snippet, part.lo, part.hi))
                    });
                    if confusion != Confusion::None {
                        message.to_mut().push_str(confusion.label_text());
                    }
                    let fold = !attribute_suggestion(file, &mut parts);
                    let Some((start, end, line)) = shrink_file(file, parts.iter().map(|it| (it.lo, it.hi)))
                    else {
                        continue;
                    };
                    subs.push(
                        Snippet::source(&file.text[start..end])
                            .line_start(line)
                            .path(shown(name))
                            .fold(fold)
                            .patches(parts.into_iter().map(move |it| {
                                Patch::new(
                                    it.lo.saturating_sub(start)..it.hi.saturating_sub(start),
                                    it.snippet,
                                )
                            })),
                    );
                }
                if !subs.is_empty() {
                    report.push(std::mem::replace(
                        &mut group,
                        Group::with_title(annotate_snippets::Level::HELP.secondary_title(message)),
                    ));
                    group = group.elements(subs);
                    if others > 0 {
                        let plural = if others == 1 { "" } else { "s" };
                        group = group.element(
                            annotate_snippets::Level::NOTE
                                .no_name()
                                .message(format!("and {others} other candidate{plural}")), // alloc: rendering, the edge
                        );
                    }
                }
            }
        }
    }
    if !group.is_empty() {
        report.push(group);
    }
    renderer.render(&report)
}

/// rustc's `add_feature_diagnostics_for_issue` under `-Z ui-testing`: the date the compiler was
/// built on is written `YYYY-MM-DD`.
fn ui_dated(message: &str, ui_testing: bool) -> Cow<'_, str> {
    use crate::lower::mutable_ref_binding::BUILT_ON;
    match ui_testing && message.starts_with("this compiler was built on ") {
        // alloc: one note of a feature gate, rendered for a UI test
        true => Cow::Owned(message.replacen(BUILT_ON, "YYYY-MM-DD", 1)),
        false => Cow::Borrowed(message),
    }
}

/// rustc's `annotation_level_for_level`.
fn level_of(level: Level) -> annotate_snippets::Level<'static> {
    match level {
        Level::Error => annotate_snippets::Level::ERROR,
        Level::Warning => annotate_snippets::Level::WARNING,
        Level::Note => annotate_snippets::Level::NOTE,
        Level::Help => annotate_snippets::Level::HELP,
        Level::FailureNote => annotate_snippets::Level::NOTE.no_name(),
    }
}

fn sublevel_of(level: Sublevel) -> annotate_snippets::Level<'static> {
    match level {
        Sublevel::Note => annotate_snippets::Level::NOTE,
        Sublevel::Help => annotate_snippets::Level::HELP,
        Sublevel::Warning => annotate_snippets::Level::WARNING,
    }
}

/// A label placed in its file: bytes of the file's text as rustc has it, and the line they
/// start on.
struct Annotation<'a> {
    kind: AnnotationKind,
    lo: usize,
    hi: usize,
    line: usize,
    label: Option<Cow<'a, str>>,
}

/// The labels of a span by file, each file's in the order rustc shows them.
type Annotated<'a> = SmallVec<[(&'a str, &'a File, SmallVec<[Annotation<'a>; 4]>); 2]>;

/// How [`snippets`] writes the labels of a file whose source rustc doesn't show.
struct Unshown {
    /// The level of the diagnostic or child the span is of, which starts the group of each such
    /// file after the first.
    level: annotate_snippets::Level<'static>,
    short: bool,
    /// The diagnostic's first child has no span: after the last file, when it is one not
    /// shown, a padding line comes before it.
    pad_last: bool,
}

/// The snippets of `span` with `extra` labels on it, the file of its primary span first. A file
/// rustc doesn't show the source of is written as where its labels are, each label a note
/// (rustc's `unannotated_messages`), and nothing under `short`; a group that starts for one is
/// pushed to `report`.
fn snippets<'a>(
    mut group: Group<'a>,
    span: &'a MultiSpan,
    extra: Labels<'a>,
    placed: &Placed<'a>,
    report: &mut Vec<Group<'a>>,
    unshown: Unshown,
) -> Group<'a> {
    let mut files = collect_annotations(span, extra, placed);
    // As rustc finds it: by a binary search of the files in the order they were met.
    if let Some(primary) = span.primary_span()
        && let Ok(at) = files.binary_search_by(|(name, ..)| (*name).cmp(primary.file.as_str()))
    {
        files.swap(0, at);
    }
    let last = files.len().saturating_sub(1);
    for (at, (name, file, annotations)) in files.into_iter().enumerate() {
        if file.shown {
            if let Some(snippet) = annotated_snippet(name, file, annotations) {
                group = group.element(snippet);
            }
        } else if !unshown.short {
            group = unannotated(group, name, file, annotations, at, report, &unshown.level);
            if unshown.pad_last && at == last {
                group = group.element(annotate_snippets::Padding);
            }
        }
    }
    group
}

/// rustc's `unannotated_messages`: the labels of the `at`th file of a span, one whose source is
/// not shown, as the line and column of each (one per line) with its label as a note.
fn unannotated<'a>(
    mut group: Group<'a>,
    name: &'a str,
    file: &'a File,
    annotations: SmallVec<[Annotation<'a>; 4]>,
    at: usize,
    report: &mut Vec<Group<'a>>,
    level: &annotate_snippets::Level<'static>,
) -> Group<'a> {
    let path = shown(name);
    let mut lines: SmallVec<[usize; 4]> = SmallVec::new();
    for (i, annotation) in annotations.into_iter().enumerate() {
        if i != 0 && annotation.label.is_none() {
            continue;
        }
        if i == 0 && at != 0 {
            // clone: a `'static` level, for a file after the first of a diagnostic written
            report.push(std::mem::replace(&mut group, Group::with_level(level.clone())));
        }
        // Without the source rustc's display column is the column in chars.
        let (lo_line, lo_column) = file.position(annotation.lo);
        let (hi_line, hi_column) = file.position(annotation.hi);
        if !lines.contains(&lo_line) && (i == 0 || hi_line <= lo_line) {
            lines.push(lo_line);
            group = group.element(annotate_snippets::Origin::path(path).line(lo_line).char_column(lo_column));
        }
        let label = annotation.label.filter(|it| !it.is_empty());
        if hi_line > lo_line && label.is_some() && !lines.contains(&hi_line) {
            lines.push(hi_line);
            group = group.element(annotate_snippets::Origin::path(path).line(hi_line).char_column(hi_column));
        }
        if let Some(label) = label {
            group = group.element(annotate_snippets::Padding).element(annotate_snippets::Level::NOTE.message(label));
        }
    }
    group
}

/// rustc's `collect_annotations` of `MultiSpan::span_labels`: the labelled spans in the order
/// they were labelled, then the primary spans with no label, by file in the order they are
/// met, each file's by line, leaving out those not in their file.
fn collect_annotations<'a>(span: &'a MultiSpan, extra: Labels<'a>, placed: &Placed<'a>) -> Annotated<'a> {
    let mut unlabelled: SmallVec<[&'a Span; 2]> = SmallVec::new();
    for at in &span.primary {
        let labelled = span.labels.iter().any(|(it, _)| it == at) || extra.iter().any(|(it, _)| *it == at);
        if !labelled && !unlabelled.contains(&at) {
            unlabelled.push(at);
        }
    }
    let labels = span
        .labels
        .iter()
        .map(|(at, label)| (at, Some(Cow::Borrowed(label.as_str()))))
        .chain(extra.into_iter().map(|(at, label)| (at, Some(label))))
        .chain(unlabelled.into_iter().map(|at| (at, None)));
    let mut output: Annotated<'a> = SmallVec::new();
    for (at, label) in labels {
        let file = placed.file_of(at);
        if !file.holds(at) {
            continue;
        }
        let lo = file.norm(at.lo);
        let is_primary = label.is_none() || span.primary.contains(at);
        let annotation = Annotation {
            kind: if is_primary { AnnotationKind::Primary } else { AnnotationKind::Context },
            lo,
            hi: file.norm(at.hi),
            line: file.line_of(lo),
            label: label.map(normalize_whitespace),
        };
        match output.iter_mut().find(|(name, ..)| *name == at.file) {
            Some((.., annotations)) => annotations.push(annotation),
            // alloc: none, the annotation inline in the `SmallVec`
            None => output.push((at.file.as_str(), file, smallvec![annotation])),
        }
    }
    for (.., annotations) in &mut output {
        radix::sort_by_key(annotations, |it| it.line);
    }
    output
}

fn annotated_snippet<'a>(
    name: &'a str,
    file: &'a File,
    annotations: SmallVec<[Annotation<'a>; 4]>,
) -> Option<Snippet<'a, annotate_snippets::Annotation<'a>>> {
    let (start, end, line) = shrink_file(file, annotations.iter().map(|it| (it.lo, it.hi)))?;
    Some(Snippet::source(&file.text[start..end]).line_start(line).path(shown(name)).annotations(
        annotations.into_iter().map(move |it| {
            let annotation = it.kind.span(it.lo.saturating_sub(start)..it.hi.saturating_sub(start));
            match it.label {
                Some(label) => annotation.label(label),
                None => annotation,
            }
        }),
    ))
}

/// rustc's `shrink_file`: where the whole lines `ranges` are on start and end, and the line
/// number of the first.
fn shrink_file(file: &File, ranges: impl Iterator<Item = (usize, usize)>) -> Option<(usize, usize, usize)> {
    let (lo, hi) = ranges.reduce(|(lo, hi), (a, b)| (lo.min(a), hi.max(b)))?;
    let lo_line = file.line_of(lo);
    let start = file.line_bounds(lo_line).start;
    let end = file.line_bounds(file.line_of(hi)).end;
    Some((start, end, lo_line + 1))
}

/// A part of a way to substitute, placed in its file's text.
struct Part<'a> {
    lo: usize,
    hi: usize,
    snippet: Cow<'a, str>,
}

/// The ways to substitute of a suggestion that rustc shows: those whose parts are all in one
/// file it holds them in and shows, each with its parts in order, leaving out the parts that change
/// nothing, and the ways left with no part.
fn substitutions<'a>(
    substitutions: &'a [Vec<(Span, String)>],
    placed: &Placed<'a>,
) -> Vec<(&'a str, &'a File, SmallVec<[Part<'a>; 2]>)> {
    let mut out = Vec::with_capacity(substitutions.len());
    for parts in substitutions {
        let Some((first, _)) = parts.first() else { continue };
        let name = first.file.as_str();
        if parts.iter().any(|(span, _)| span.file != name) {
            continue;
        }
        let Some(file) = placed.get(name).filter(|it| it.shown) else { continue };
        if parts.iter().any(|(span, _)| !file.holds(span)) {
            continue;
        }
        let mut kept: SmallVec<[Part<'a>; 2]> = parts
            .iter()
            .map(|(span, snippet)| Part {
                lo: file.norm(span.lo),
                hi: file.norm(span.hi),
                snippet: Cow::Borrowed(snippet.as_str()),
            })
            .collect(); // alloc: none up to 2 parts, inline in the `SmallVec`; rendering, the edge
        radix::sort_by_key(&mut kept, |it| it.lo);
        kept.retain(|it| is_different(file, &it.snippet, it.lo, it.hi));
        if !kept.is_empty() {
            out.push((name, file, kept));
        }
    }
    out
}

/// rustc's attribute special case: a suggestion that adds `#[...]` on a line of its own is
/// moved to the start of the line, with its indentation, and shown with the line after.
fn attribute_suggestion(file: &File, parts: &mut [Part<'_>]) -> bool {
    let [part] = parts else { return false };
    let trimmed = part.snippet.trim();
    if !(trimmed.starts_with("#[") && trimmed.ends_with(']') && part.snippet.ends_with('\n')) {
        return false;
    }
    if part.lo != part.hi {
        return false;
    }
    let before = &file.text[..part.lo];
    let before = before.rsplit_once('\n').map_or(before, |(_, it)| it);
    if !before.trim().is_empty() {
        return false;
    }
    if !before.is_empty() && !part.snippet.ends_with(before) {
        part.snippet.to_mut().insert_str(0, before);
        part.lo -= before.len();
        part.hi = part.lo;
    }
    true
}

/// rustc's `render_multispan_macro_backtrace` without `-Z macro-backtrace`: a primary span in a
/// macro's expansion that is not in its call gets a label on the outermost call of a macro
/// whose definition rustc knows.
fn macro_labels<'a>(span: &'a MultiSpan, labels: &mut Labels<'a>) {
    for primary in &span.primary {
        let Some(expansion) = primary.macro_backtrace().filter(|it| it.def_site.is_some()).last() else {
            continue;
        };
        let call = &expansion.call_site;
        if call.file == primary.file && call.lo <= primary.lo && primary.hi <= call.hi {
            continue;
        }
        let label = match expansion.kind {
            MacroKind::Attr => "in this attribute macro expansion",
            MacroKind::Derive => "in this derive macro expansion",
            MacroKind::Bang => "in this macro invocation",
        };
        if !labels.iter().any(|(at, it)| *at == call && *it == label) {
            labels.push((call, Cow::Borrowed(label)));
        }
    }
}

/// rustc's `primary_span_formatted`: a lone suggestion that is short is shown as a label on
/// what it replaces, and not as a patch.
fn primary_span_formatted<'a>(
    suggestions: &'a [Suggestion],
    placed: &Placed<'a>,
) -> Option<(&'a Span, Cow<'a, str>)> {
    let [suggestion] = suggestions else { return None };
    let [substitution] = &suggestion.substitutions[..] else { return None };
    let [(part_span, snippet)] = &substitution[..] else { return None };
    let inline = suggestion.message.split_whitespace().count() < 10
        && !snippet.contains('\n')
        && !matches!(
            suggestion.style,
            SuggestionStyle::HideCodeAlways
                | SuggestionStyle::CompletelyHidden
                | SuggestionStyle::ShowAlways
        );
    if !inline {
        return None;
    }
    let snippet = snippet.trim();
    let label = if snippet.is_empty() || suggestion.style.hide_inline() {
        format!("help: {}", suggestion.message) // alloc: a suggestion rendered, the edge
    } else {
        let confusion = match placed.get(&part_span.file).and_then(|it| it.snippet(part_span)) {
            Some(found) => confusion_of(found, snippet),
            None => Confusion::None,
        };
        // alloc: a suggestion rendered, the edge
        format!("help: {}{}: `{snippet}`", suggestion.message, confusion.label_text())
    };
    Some((part_span, Cow::Owned(label)))
}

/// rustc's `is_different`: whether `suggested` is not what is at `lo..hi` already.
fn is_different(file: &File, suggested: &str, lo: usize, hi: usize) -> bool {
    file.slice(lo, hi).is_none_or(|found| found != suggested)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confusion {
    None,
    Case,
    DigitLetter,
    Both,
}

impl Confusion {
    fn label_text(self) -> &'static str {
        match self {
            Confusion::None => "",
            Confusion::Case => " (notice the capitalization)",
            Confusion::DigitLetter => " (notice the digit/letter confusion)",
            Confusion::Both => " (notice the capitalization and digit/letter confusion)",
        }
    }

    fn combine(self, other: Confusion) -> Confusion {
        match (self, other) {
            (Confusion::None, other) => other,
            (this, Confusion::None) => this,
            (Confusion::Both, _) | (_, Confusion::Both) => Confusion::Both,
            (Confusion::Case, Confusion::DigitLetter)
            | (Confusion::DigitLetter, Confusion::Case) => Confusion::Both,
            (this, _) => this,
        }
    }
}

/// rustc's `detect_confusion_type` of the text at `lo..hi`.
fn detect_confusion_type(file: &File, suggested: &str, lo: usize, hi: usize) -> Confusion {
    match file.slice(lo, hi) {
        Some(found) => confusion_of(found, suggested),
        None => Confusion::None,
    }
}

/// Whether `suggested` differs from `found` only in letters that look alike.
fn confusion_of(found: &str, suggested: &str) -> Confusion {
    if found.len() != suggested.len() {
        return Confusion::None;
    }
    const ASCII_CONFUSABLES: &[char] =
        &['c', 'f', 'i', 'k', 'o', 's', 'u', 'v', 'w', 'x', 'y', 'z'];
    const DIGIT_LETTER: [(char, char); 5] =
        [('0', 'O'), ('1', 'l'), ('5', 'S'), ('8', 'B'), ('9', 'g')];
    let (mut case, mut digit_letter, mut other) = (false, false, false);
    for (f, s) in std::iter::zip(found.chars(), suggested.chars()) {
        if f == s {
            continue;
        }
        if f.eq_ignore_ascii_case(&s) {
            if ASCII_CONFUSABLES.contains(&f) || ASCII_CONFUSABLES.contains(&s) {
                case = true;
            } else {
                other = true;
            }
        } else if DIGIT_LETTER.contains(&(f, s)) || DIGIT_LETTER.contains(&(s, f)) {
            digit_letter = true;
        } else {
            other = true;
        }
    }
    let differs = found != suggested;
    match (case && !other && differs, digit_letter && !other && differs) {
        (true, true) => Confusion::Both,
        (true, false) => Confusion::Case,
        (false, true) => Confusion::DigitLetter,
        (false, false) => Confusion::None,
    }
}

/// rustc's `OUTPUT_REPLACEMENTS`, in its order.
const OUTPUT_REPLACEMENTS: &[(char, &str)] = &[
    ('\0', "␀"),
    ('\u{0001}', "␁"),
    ('\u{0002}', "␂"),
    ('\u{0003}', "␃"),
    ('\u{0004}', "␄"),
    ('\u{0005}', "␅"),
    ('\u{0006}', "␆"),
    ('\u{0007}', "␇"),
    ('\u{0008}', "␈"),
    ('\t', "    "),
    ('\u{000b}', "␋"),
    ('\u{000c}', "␌"),
    ('\u{000d}', "␍"),
    ('\u{000e}', "␎"),
    ('\u{000f}', "␏"),
    ('\u{0010}', "␐"),
    ('\u{0011}', "␑"),
    ('\u{0012}', "␒"),
    ('\u{0013}', "␓"),
    ('\u{0014}', "␔"),
    ('\u{0015}', "␕"),
    ('\u{0016}', "␖"),
    ('\u{0017}', "␗"),
    ('\u{0018}', "␘"),
    ('\u{0019}', "␙"),
    ('\u{001a}', "␚"),
    ('\u{001b}', "␛"),
    ('\u{001c}', "␜"),
    ('\u{001d}', "␝"),
    ('\u{001e}', "␞"),
    ('\u{001f}', "␟"),
    ('\u{007f}', "␡"),
    ('\u{200d}', ""),
    ('\u{202a}', "�"),
    ('\u{202b}', "�"),
    ('\u{202c}', "�"),
    ('\u{202d}', "�"),
    ('\u{202e}', "�"),
    ('\u{2066}', "�"),
    ('\u{2067}', "�"),
    ('\u{2068}', "�"),
    ('\u{2069}', "�"),
];

/// rustc's `normalize_whitespace`, borrowing a text that has none of the chars it replaces:
/// each is below U+0020, U+007F, or three bytes starting with `0xE2`, found by a scan of the
/// bytes that vectorizes.
fn normalize_whitespace(text: Cow<'_, str>) -> Cow<'_, str> {
    if !text.bytes().any(|b| b < 0x20 || b == 0x7f || b == 0xe2) {
        return text;
    }
    Cow::Owned(text.chars().fold(String::with_capacity(text.len()), |mut out, c| {
        match OUTPUT_REPLACEMENTS.binary_search_by_key(&c, |(it, _)| *it) {
            Ok(at) => out.push_str(OUTPUT_REPLACEMENTS[at].1),
            Err(_) => out.push(c),
        }
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts(file: &File) -> Vec<u32> {
        (0..file.lines.line_count()).map(|line| file.lines.line_start(line).unwrap().into()).collect()
    }

    #[test]
    fn a_file_is_measured_as_rustc_has_it_without_its_bom_and_crs() {
        let file = File::new("\u{feff}a\r\nbc\r\n\r\nd".to_owned());
        assert_eq!(file.text, "a\nbc\n\nd");
        assert_eq!(starts(&file), [0, 2, 5, 6]);
        // `b`, after the BOM (3), `a` and `\r\n`.
        assert_eq!(file.norm(6), 2);
        assert_eq!(file.original(2), 6);
        // The end of `a`, where `\r` is, is the `\n` of the text, and back.
        assert_eq!(file.norm(4), 1);
        assert_eq!(file.original(1), 4);
        assert_eq!(file.norm(0), 0);
        assert_eq!(file.original(0), 3);
        assert_eq!(file.position(file.norm(6)), (2, 0));
    }

    #[test]
    fn a_file_without_a_bom_or_crs_is_not_copied() {
        let read = "fn f() {}\n".to_owned();
        let at = read.as_ptr();
        let file = File::new(read);
        assert_eq!(file.text.as_ptr(), at);
        assert!(file.ascii);
        assert!(file.crs.is_empty());
    }

    #[test]
    fn a_trailing_newline_starts_no_line() {
        let file = File::new("ab\ncd\n".to_owned());
        assert_eq!(starts(&file), [0, 3]);
        assert_eq!(file.position(6), (2, 3));
    }

    #[test]
    fn positions_are_one_based_in_chars() {
        let sources = Sources::of([("a.rs".to_owned(), "é = 1;\n  x".to_owned())]);
        assert_eq!(sources.position("a.rs", 2), Some((1, 2)));
        assert_eq!(sources.position("a.rs", 10), Some((2, 3)));
        assert_eq!(sources.position("a.rs", 11), Some((2, 4)));
        assert_eq!(sources.position("a.rs", 12), None);
        assert_eq!(sources.position("b.rs", 0), None);
    }

    #[test]
    fn unread_names_each_file_a_batch_points_into_once() {
        let at = |file: &str, expansion: Option<crate::model::Expansion>| Span {
            file: file.to_owned(),
            lo: 0,
            hi: 1,
            expansion: expansion.map(Box::new),
        };
        let expanded = at(
            "a.rs",
            Some(crate::model::Expansion {
                call_site: at("c.rs", None),
                kind: MacroKind::Bang,
                name: "m".to_owned(),
                def_site: Some(at("d.rs", None)),
                opaque: false,
            }),
        );
        let mut first = Diag::error(None, "x", expanded);
        first.span_note(at("b.rs", None), "here");
        let second = Diag::error(None, "y", at("b.rs", None));
        let sources = Sources::of([("c.rs".to_owned(), "x".to_owned())]);
        assert_eq!(sources.unread([&first, &second]), ["a.rs", "d.rs", "b.rs"]);
    }

    #[test]
    fn a_file_added_is_placed_and_one_that_cannot_be_read_is_not() {
        let mut sources = Sources::default();
        sources.add("a.rs", Some("fn f() {}\n".to_owned()));
        sources.add("a.rs", Some("other".to_owned()));
        sources.add("b.rs", None);
        assert_eq!(sources.position("a.rs", 3), Some((1, 4)));
        assert_eq!(sources.position("a.rs", 4), Some((1, 5)));
        assert_eq!(sources.position("b.rs", 0), None);
        assert_eq!(sources.position("c.rs", 0), None);
        let diag = Diag::error(None, "x", Span { file: "b.rs".to_owned(), lo: 0, hi: 0, expansion: None });
        assert!(sources.unread([&diag]).is_empty());
    }

    #[test]
    fn confusables_are_noticed() {
        assert_eq!(confusion_of("Vec", "vec"), Confusion::Case);
        assert_eq!(confusion_of("l0", "1O"), Confusion::DigitLetter);
        assert_eq!(confusion_of("sO", "S0"), Confusion::Both);
        assert_eq!(confusion_of("so", "S0"), Confusion::None, "only `O` is confused with `0`");
        assert_eq!(confusion_of("abc", "abd"), Confusion::None);
    }

    #[test]
    fn text_without_chars_to_replace_is_borrowed() {
        assert!(matches!(normalize_whitespace(Cow::Borrowed("expected `u32`, 中文")), Cow::Borrowed(_)));
        assert_eq!(normalize_whitespace(Cow::Borrowed("a\tb\u{200d}c")), "a    bc");
        // `’` starts with 0xE2 too and is kept as it is.
        assert_eq!(normalize_whitespace(Cow::Borrowed("it’s")), "it’s");
    }

    /// Two files with no path shown by one name: each span reads its own file's text from its
    /// name, none is asked of the caller, and the name shown is the name alone.
    #[test]
    fn files_shown_by_one_name_keep_their_own_text() {
        let a = name_with_text("<crate attribute>", "#![(]");
        let b = name_with_text("<crate attribute>", "#![feature(foo),feature(bar)]");
        assert_ne!(a, b);
        assert_eq!((shown(&a), shown(&b), shown("src/lib.rs")), ("<crate attribute>", "<crate attribute>", "src/lib.rs"));
        let first = Diag::error(None, "x", Span { file: a.clone(), lo: 3, hi: 4, expansion: None });
        let second = Diag::error(None, "y", Span { file: b.clone(), lo: 15, hi: 16, expansion: None });
        let sources = Sources::default();
        assert!(sources.unread([&first, &second]).is_empty());
        assert_eq!(sources.position(&a, 3), Some((1, 4)));
        assert_eq!(sources.position(&b, 15), Some((1, 16)));
        assert_eq!(sources.position(&a, 6), None);
        let line = json(&second, &sources, Style::default()).unwrap();
        assert!(line.contains(r#""file_name":"<crate attribute>""#), "{line}");
        assert!(line.contains(r##""text":"#![feature(foo),feature(bar)]""##), "{line}");
        let style = Style { ui_testing: true, ..Style::default() };
        let (first, second) = (rendered(&first, &sources, style).unwrap(), rendered(&second, &sources, style).unwrap());
        assert!(first.contains("--> <crate attribute>:1:4\n") && first.contains("LL | #![(]\n"), "{first}");
        assert!(!first.contains("feature"), "{first}");
        assert!(second.contains("--> <crate attribute>:1:16\n"), "{second}");
        assert!(second.contains("LL | #![feature(foo),feature(bar)]\n"), "{second}");
        assert!(!first.contains('\0') && !second.contains('\0'));
    }

    #[test]
    fn a_primary_span_given_twice_is_marked_once() {
        let sources = Sources::of([("a.rs".to_owned(), "let x = 1;\n".to_owned())]);
        let at = Span { file: "a.rs".to_owned(), lo: 4, hi: 5, expansion: None };
        let once = Diag::error(None, "x", at.clone());
        let mut twice = once.clone();
        twice.primary(at);
        assert_eq!(
            rendered(&twice, &sources, Style::default()).unwrap(),
            rendered(&once, &sources, Style::default()).unwrap()
        );
    }
}
