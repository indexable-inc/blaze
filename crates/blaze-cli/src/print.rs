//! Findings placed when printed (drive.md, "Printing"; rule 71): a finding holds its item's
//! start and an item-relative range; its frame (or line) is drawn here by blaze-report, from the
//! file's text, and nowhere else. And the profile (rule 17): written to `.blaze/v2/profile.txt`
//! on every command, its summary line on stderr under `--stats`.

use blaze_drive::condition::OVER_CAP_RULE;
use blaze_drive::{Answer, Print, Request};
use blaze_ladder::refusal::Refusal;
use blaze_own::{AString, Vec};
use blaze_report::Severity;
use blaze_report::frame::plain::{self, Plain, PlainClosing, View};
use blaze_report::frame::words::{self, Area, HelpKind, WordsError};
use blaze_report::frame::{Caps, Closing, Counts, Coverage, Edit, Finding, Fixes, Form, Input, LINK_FILE, Mark, Source, closing as closing_line, render};

use crate::fix::{Change, Id, Listed};
use bstd::cx::Cx;
use bstd::fs::{Dir, Durable, ReadWrite};
use bstd::mem::{Bytes, Frame, format_in};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;
use crate::phases::Phases;

/// What a command has drawn so far: the terminal's abilities it draws for (built once in main,
/// `blaze_report::frame::detect`), the counts its closing line says, and the files they were in.
pub struct Shown<'f> {
    pub caps: Caps,
    pub counts: Counts,
    /// The view the command prints in (D70), chosen once at its edge.
    pub view: View,
    /// `--fix`'s first check: its findings are listed, nothing is printed.
    pub quiet: bool,
    /// The request's findings, placed in today's text, with their ids and fixes.
    pub listed: Vec<Listed<'f>, &'f Frame>,
    /// The ids of the findings printed with a fix: the plain closing line's command.
    pub fixes: Vec<Id, &'f Frame>,
    /// The checkout's absolute path: a frame's header links to `root/path`.
    root: &'f str,
    /// The files drawn in, by their name's subject: one count a file, whichever stage found it.
    files: Vec<blaze_ids::ContentId, &'f Frame>,
    /// How many bodies said their types were not checked (body_types.rs): the types stage then
    /// did not run, whatever else was found (main.rs `close`).
    pub types_unchecked: u32,
    /// Each item the check could not check, with its cause (not_checked/items.rs `keep`): the
    /// cause lines, the summary line and the exit status (main.rs `close`). Its words are the
    /// listed finding's, borrowed: a row here is three borrows, nothing copied.
    pub unchecked: Vec<crate::not_checked::items::Unchecked<'f>, &'f Frame>,
    /// How many bodies said their moves and borrows were not checked (body_types.rs
    /// `BORROWS_NOT_CHECKED`): with `BORROWS_ARE_ERRORS`, the borrows stage then did not run.
    pub borrows_unchecked: u32,
    /// Each task not run because an input it reads could not be made (work.rs `NOT_RUN`), with
    /// its cause (not_checked/tasks.rs `keep`): one line a cause and the exit status (main.rs
    /// `close`); never drawn or counted one by one.
    pub not_run: Vec<crate::not_checked::tasks::NotRun<'f>, &'f Frame>,
    /// The command's lint step (lints.rs `lints_of`): a lint finding's level is its levels' at
    /// print time (rule 81), never stored. `Step::Rust` until the command sets it.
    pub lints: blaze_lint::Step,
    /// `--each`: every finding that is only a consequence (a task waiting on a failed one, an
    /// item or a body not checked) drawn one by one, as well as counted under its cause. Off,
    /// they are counted and grouped only, and never placed in their files.
    pub each: bool,
    /// The items not checked because a task failed (not_checked/waited.rs), as the lines under
    /// the failures counted them, and how many failures' lines counted any: the summary line
    /// counts them beside the items not checked for their own causes (main.rs `close`), so it
    /// never reads better than the truth.
    pub behind: crate::not_checked::Behind,
}

impl<'f> Shown<'f> {
    /// The plain views are lines whatever the terminal draws: their bytes are the same anywhere.
    pub fn new(caps: Caps, view: View, root: &'f str, frame: &'f Frame) -> Shown<'f> {
        let caps = if view == View::Person { caps } else { Caps::LINES };
        Shown {
            caps,
            counts: Counts::default(),
            view,
            quiet: false,
            listed: Vec::new_in(frame),
            fixes: Vec::new_in(frame),
            root,
            files: Vec::new_in(frame),
            types_unchecked: 0,
            unchecked: Vec::new_in(frame),
            borrows_unchecked: 0,
            not_run: Vec::new_in(frame),
            lints: blaze_lint::Step::Rust,
            each: false,
            behind: crate::not_checked::Behind::default(),
        }
    }

    fn file(&mut self, path: &str) {
        let subject = blaze_drive::Name::file(path.as_bytes()).subject;
        if !self.files.contains(&subject) {
            self.files.push(subject);
            self.counts.files += 1;
        }
    }
}

/// The file a finding of the request is drawn in: its slot's real path (sweep.rs, a module at
/// `x/mod.rs`), or none for a text-less name.
pub fn file_of<'f>(req: &Request<'_>, found: &crate::sweep::Found<'_>, finding: &Print<'_>, frame: &'f Frame) -> Result<Option<&'f str>, CliError> {
    let Some(index) = finding.file else { return Ok(None) };
    let slot = req.path(index);
    let real = crate::sweep::real_path(core::str::from_utf8(slot).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?, found.flags(slot), frame);
    core::str::from_utf8(real.leak()).map(Some).map_err(|_| CliError::Text { what: "a path is not UTF-8" })
}

/// A finding drawn as its frame (or, off a terminal, its one line): its rule's words from
/// blaze-report's table, its arguments the message's pieces between 0x1F bytes.
/// `path` is the file it is drawn in ([`file_of`]), `place` its item's start there and
/// `message` its words' pieces (the finding's own, but for a finding that carries its place in
/// its message: names.rs `placed`).
/// A finding's code (`syntax`, `E0425`, a lint's name): blaze-report's wording's, or for a lint
/// (lints.rs, numbered past `LINT_BASE`) its rule's name, which blaze-report has no row of.
pub(crate) fn code_of(area: Area, rule: u16) -> &'static str {
    match crate::lints::rule_of(rule) {
        Some(lint) => lint.name(),
        None => words::wording(area, rule).map_or("", |wording| wording.name),
    }
}

/// The area a request's finding's rule is numbered in.
pub fn area_of(finding: &Print<'_>) -> Area {
    match finding.rule {
        OVER_CAP_RULE | blaze_drive::request::MODULE_FILE_MISSING_RULE | blaze_drive::request::PATH_FILE_MISSING_RULE => Area::Drive,
        _ => Area::Cli,
    }
}

/// One finding placed in its file's `text` ([`list`]): `place` its item's start there, `range`
/// its bytes from that start (the finding's own `start..end`, or its token place's bytes today:
/// [`Place::Tokens`]), `grammar` the file's outline when it is Rust, `read` the text's digest.
#[expect(clippy::too_many_arguments, reason = "one finding's facts, each a different one")]
fn placed_in<'f>(path: &'f str, text: &str, read: blaze_ids::ContentId, grammar: Option<&blaze_file_items::Outline<'_>>, place: u32, range: (u32, u32), message: &'f [u8], finding: &Print<'_>) -> Result<Listed<'f>, CliError> {
    let area = area_of(finding);
    let code = code_of(area, finding.rule);
    let start = place.saturating_add(range.0).min(text.len() as u32);
    let end = place.saturating_add(range.1).clamp(start, text.len() as u32);
    let (item, item_end, chain) = crate::fix::item_in(grammar, text, start);
    let id = Id::of(path.as_bytes(), chain, &text.as_bytes()[item as usize..item_end as usize], code, (start - item, end - item));
    let fix = match (area, finding.rule) {
        (Area::Cli, crate::units::TOOLCHAIN_NAMED) => {
            let name = core::str::from_utf8(message).map_err(|_| CliError::Text { what: "a finding's message is not UTF-8" })?;
            Some(Change { start, end, insert: "", reason: HelpKind::ToolchainLine, args: [name, ""], given: 1 })
        }
        _ => None,
    };
    Ok(Listed { path: Some(path), about: finding.name, read, area, rule: finding.rule, words: message, item, start, end, id, fix })
}

/// A finding of no file (or whose item is no longer in its file), listed at its name: its id
/// is of the name. `one` holds the name's text while the id is made.
/// `no_text` is the digest of no bytes, made once by the caller.
fn unplaced<'f>(message: &'f [u8], finding: &Print<'_>, no_text: blaze_ids::ContentId, one: &Frame) -> Listed<'f> {
    let area = area_of(finding);
    let code = code_of(area, finding.rule);
    let about = format_in!(one, "{}", finding.name);
    let id = Id::of(about.as_bytes(), 0, &[], code, (finding.start, finding.end));
    Listed { path: None, about: finding.name, read: no_text, area, rule: finding.rule, words: message, item: 0, start: finding.start, end: finding.end, id, fix: None }
}

/// A finding only counted under its cause ([`Place::Counted`]): listed with its file's path and
/// its words, and nothing made for it (no id, no name's text, no file read). Tempo's no-op
/// check has 31,000 such findings beside 626 drawn.
fn counted<'f>(wanted: &Wanted<'f>, finding: &Print<'_>, no_text: blaze_ids::ContentId) -> Listed<'f> {
    Listed { path: wanted.path, about: finding.name, read: no_text, area: area_of(finding), rule: finding.rule, words: wanted.words, item: 0, start: finding.start, end: finding.end, id: Id::UNSHOWN, fix: None }
}

/// The memory a command's findings are placed and drawn in. One file's text and its outlines
/// are in `file`, freed when the next file is read; one finding's drawing is in `one`, freed a
/// finding. Nothing of a file or of a drawing is made in the command's frame, so what a command
/// can list is bounded by the count of its findings (a [`Listed`], its words and a path a
/// file), never by the sizes of their files or frames. (Before: each finding kept four copies
/// of its file and two outlines in the command's frame, and blaze's own 3,487 findings filled
/// its 64 MiB: drive.md, runner 13.)
pub struct Scratch {
    file: Frame,
    one: Frame,
    limit: usize,
    /// What the two frames held at their most, and the floor under it ([`Held`]).
    held: Held,
    /// The frames' `used` where the current file's work and the current line began: what is
    /// past them is that file's and that line's alone.
    from: (usize, usize),
}

/// The print path's memory (the tail's bound): it is STREAMED, so what it holds at once is one
/// file's work (its text, and its outline while placing) in the file frame, emptied per file,
/// and one line's drawing in the one frame, emptied per line. The floor any printer of these
/// lines holds is one file's work and one line, at their largest ([`Held::floor`]): each is
/// measured from where it began, so it is that file's or that line's alone, whatever the frames
/// held before. `peak` is what both frames held together at their most (their whole `used`).
/// Streamed, `peak <= floor`; a printer that kept the texts or the lines holds their sum. The
/// per-finding lists (`Listed`, `Wanted`, the one-of-two set) are not here: they are the
/// findings' own size, counted apart (engine_lines.rs `Tail`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    /// Both frames' bytes together at their most.
    pub peak: usize,
    /// The most one file's work took (its text and outlines).
    pub file: usize,
    /// The largest file text read (a part of `file`).
    pub text: usize,
    /// The most one finding's drawing (or placing) took.
    pub line: usize,
}

impl Held {
    /// The least any printer of these lines holds at once: one file's work and one line.
    pub fn floor(&self) -> usize {
        self.file.saturating_add(self.line)
    }

    /// After one finding: the frames as they stand, the file's work and the line from `from`.
    fn saw(&mut self, file: &Frame, one: &Frame, from: (usize, usize), text: usize) {
        self.peak = self.peak.max(file.used().saturating_add(one.used()));
        self.file = self.file.max(file.used().saturating_sub(from.0));
        self.line = self.line.max(one.used().saturating_sub(from.1));
        self.text = self.text.max(text);
    }
}

#[cfg(test)]
std::thread_local! {
    /// The planted control of the tail's bound ([`with_frames_kept`]): the frames never emptied.
    static KEPT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// `run` with the print path's frames never emptied (rule 23's control for the tail's bound):
/// every text and every line stays held to the end, the same bytes printed.
#[cfg(test)]
pub(crate) fn with_frames_kept<R>(run: impl FnOnce() -> R) -> R {
    struct Back;
    impl Drop for Back {
        fn drop(&mut self) {
            KEPT.with(|kept| kept.set(false));
        }
    }
    KEPT.with(|kept| kept.set(true));
    let _back = Back;
    run()
}

/// A scratch frame emptied for the next file or finding: the one place the print path lets go.
fn emptied(frame: &mut Frame) {
    #[cfg(test)]
    if KEPT.with(core::cell::Cell::get) {
        return;
    }
    frame.reset();
}

/// What a [`Scratch`] frame says it is when a file does not fit it.
const FILE_ARENA: &str = "the frame for one file's findings";

impl Scratch {
    /// Two frames of `limit` bytes each, reserved: no memory is taken until used.
    pub fn new(limit: usize) -> Result<Scratch, CliError> {
        let frame = |what| Frame::with_reserve(limit, bstd::mem::Tag::Other).map_err(|_| CliError::Text { what });
        Ok(Scratch { file: frame("the frame for one file's findings could not be reserved")?, one: frame("the frame for one finding's drawing could not be reserved")?, limit, held: Held::default(), from: (0, 0) })
    }

    /// The frame of one drawing, emptied: for a drawing that reads no file of its own. The
    /// drawing before it is measured first.
    pub fn one(&mut self) -> &Frame {
        self.held.saw(&self.file, &self.one, self.from, 0);
        emptied(&mut self.one);
        self.from.1 = self.one.used();
        &self.one
    }

    /// What the frames held at their most so far, and the floor under it.
    pub fn held(&self) -> Held {
        let mut held = self.held;
        held.saw(&self.file, &self.one, self.from, 0);
        held
    }

    /// The file frame emptied for the next file's work, and where that work begins.
    fn next_file(&mut self) -> usize {
        emptied(&mut self.file);
        self.from.0 = self.file.used();
        self.from.0
    }

    /// Why the work on `path`'s findings stopped, from the frames as the stop left them.
    fn stopped(&self, doing: &'static str, path: &str) -> CliError {
        let (path, cut) = path_shown(path);
        CliError::Draw(DrawError::Stopped { doing, path, cut, file: self.file.used(), one: self.one.used(), limit: self.limit })
    }
}

/// The most of a path a [`DrawError`] holds.
const PATH_SHOWN: usize = 160;

/// A path as a [`DrawError`] holds it, and whether its start was cut: its end, when it is
/// longer than [`PATH_SHOWN`] (the file's own name is there). Made without memory: the error
/// may be that there is none.
fn path_shown(path: &str) -> (blaze_own::FixedString<PATH_SHOWN>, bool) {
    let mut cut = path.len().saturating_sub(PATH_SHOWN);
    while !path.is_char_boundary(cut) {
        cut += 1;
    }
    let mut shown = blaze_own::FixedString::new();
    // Never refused: at most `PATH_SHOWN` bytes are left.
    shown.try_push_str(&path[cut..]).ok();
    (shown, cut > 0)
}

/// Why a command's findings were not all printed: each names the file (rule 19).
#[derive(Debug)]
pub enum DrawError {
    /// The work on one file's findings panicked (its message is the panic hook's, above this):
    /// the frames it had are named with what they held, because the one cause that is not a
    /// bug of blaze is a file whose outline or drawing does not fit them (rule 20).
    Stopped { doing: &'static str, path: blaze_own::FixedString<PATH_SHOWN>, cut: bool, file: usize, one: usize, limit: usize },
    /// The file was written between the listing of its findings and their drawing: their
    /// places are the old text's.
    Changed { path: blaze_own::FixedString<PATH_SHOWN>, cut: bool },
}

impl core::fmt::Display for DrawError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let dots = |cut: &bool| if *cut { ".." } else { "" };
        match self {
            DrawError::Stopped { doing, path, cut, file, one, limit } => write!(
                f,
                "{doing} the findings of `{}{}` stopped (its message is above): {FILE_ARENA} held {file} and the frame for one finding's drawing {one} of {limit} bytes each; a file's text, outlines and one drawing must fit them (print.rs `Scratch`)",
                dots(cut),
                path.as_str()
            ),
            DrawError::Changed { path, cut } => write!(f, "`{}{}` was written while blaze drew its findings: their places are the old text's; run the command again", dots(cut), path.as_str()),
        }
    }
}

/// A finding of the request before any file is read: where it will be placed.
#[derive(Clone, Copy)]
pub struct Wanted<'f> {
    /// The file it is drawn in ([`file_of`], or its message's: names.rs `place_pieces`).
    pub path: Option<&'f str>,
    pub place: Place,
    /// Its words' pieces, kept with it.
    pub words: &'f [u8],
}

/// How a finding's item is found in its file's text as it is today.
#[derive(Clone, Copy)]
pub enum Place {
    /// The item's start, as the finding's task read it.
    At(u32),
    /// The item's chain, in the outline of this edition (a crate's unresolved path).
    Chain(u64, blaze_surface::Edition),
    /// A body's finding by its tokens (lane K1, rule 71): the item found by its chain in the
    /// outline of this edition, then the place's tokens made bytes of the item's text as it is
    /// now (blaze-file-items `place_bytes`), so a comment edit above the finding inside its
    /// body moves the printed line with no body entered.
    Tokens(u64, blaze_surface::Edition, parser::token_key::TokenPlace),
    /// Not placed: a finding only counted under its cause (not drawn without `--each`), listed
    /// with its file's path and its task's name, its file never read for it.
    Counted,
}

/// Whether a finding of `rule` is only counted under its cause unless `--each` is given: a body
/// or a borrow check not done, or a task not run. Their files are not read to place them.
pub fn counted_only(rule: u16) -> bool {
    rule == crate::body_types::TYPES_NOT_CHECKED || rule == crate::body_types::BORROWS_NOT_CHECKED || rule == crate::work::NOT_RUN
}

/// Every finding of the request placed in today's text ([`Listed`]), pushed on `into` in the
/// order of `prints`: its item found, its id made (its shown length is set once all are
/// listed, `fix::lengths`), and its fix where one is certain: today, the toolchain's crate
/// named again in a manifest, its line deleted. A file at a time, whatever the order they came
/// in: its text is read once and its outline made once for all its findings, in `scratch`.
pub fn list<'f>(cx: &Cx, root: &Dir<ReadWrite>, prints: &[Print<'_>], wanted: &[Wanted<'f>], scratch: &mut Scratch, frame: &'f Frame, into: &mut Vec<Listed<'f>, &'f Frame>) -> Result<(), CliError> {
    const NO_ROOM: CliError = CliError::Text { what: "the command's frame has no room to list its findings: a finding takes a `Listed`, its words and (once a file) its path there" };
    let first = into.len();
    into.try_reserve(prints.len()).map_err(|_| NO_ROOM)?;
    let no_text = crate::fix::text_id(&[]);
    // Each finding at its own index, first as counted ([`counted`]: nothing made); the placed
    // ones are overwritten below. The counted-only findings ([`Place::Counted`]) stay so: no
    // file is read, nothing hashed or sorted for them.
    let is_counted = |i: usize| matches!(wanted[i].place, Place::Counted);
    into.extend(wanted.iter().zip(prints.iter()).map(|(wanted, finding)| counted(wanted, finding, no_text)));
    // The placed, by file (stable: a file's findings stay in the order they came), the
    // fileless first.
    let mut order: Vec<usize, &Frame> = Vec::try_with_capacity_in(prints.len(), frame).map_err(|_| NO_ROOM)?;
    order.extend((0..prints.len()).filter(|&i| !is_counted(i)));
    order.sort_by_key(|&i| wanted[i].path);
    let mut at = 0;
    while at < order.len() {
        let path = wanted[order[at]].path;
        let run = &order[at..at + order[at..].iter().take_while(|&&i| wanted[i].path == path).count()];
        at += run.len();
        let Some(path) = path else {
            for &i in run {
                into[first + i] = unplaced(wanted[i].words, &prints[i], no_text, scratch.one());
            }
            continue;
        };
        let file_at = scratch.next_file();
        let (file, one, held) = (&scratch.file, &mut scratch.one, &mut scratch.held);
        let placed = crate::work::unwinding(|| -> Result<(), CliError> {
            let mut bytes = Bytes::new_in(file, FILE_ARENA);
            root.read_all(cx, &Path::new(path)?, &mut bytes)?;
            // A file that is not text has no item to place a finding at (its own finding, a
            // filed `FileItems` failure, says so): each is listed by its task's name alone.
            let Ok(text) = core::str::from_utf8(bytes.as_slice()) else {
                for &i in run {
                    emptied(one);
                    let line_at = one.used();
                    into[first + i] = unplaced(wanted[i].words, &prints[i], no_text, one);
                    held.saw(file, one, (file_at, line_at), bytes.len());
                }
                return Ok(());
            };
            let read = crate::fix::text_id(text.as_bytes());
            let erased = blaze_own::Erased::new(file);
            // The outline every id of the file is made from: one grammar for every file
            // (fix.rs `ITEM_GRAMMAR`), so an id is a function of the text alone.
            let grammar = path.ends_with(".rs").then(|| blaze_file_items::outline(text, crate::fix::ITEM_GRAMMAR, erased.handle()));
            // The outline a chain is looked up in is its crate's edition's: made once for each
            // edition the file's findings name (one, but for a file two crates share).
            let mut of_edition: Option<(blaze_surface::Edition, blaze_file_items::Outline<'_>)> = None;
            // The item whose chain is `chain` in the outline of `edition`: its start today.
            let mut by_chain = |chain: u64, edition: blaze_surface::Edition| match &grammar {
                Some(outline) if edition == crate::fix::ITEM_GRAMMAR => crate::work::item_by_chain(outline, text, chain),
                _ => {
                    if of_edition.as_ref().is_none_or(|(made, _)| *made != edition) {
                        of_edition = Some((edition, blaze_file_items::outline(text, edition, erased.handle())));
                    }
                    of_edition.as_ref().and_then(|(_, outline)| crate::work::item_by_chain(outline, text, chain))
                }
            };
            for &i in run {
                emptied(one);
                let line_at = one.used();
                let own = (prints[i].start, prints[i].end);
                // The item's start and the finding's bytes from it.
                let place = match wanted[i].place {
                    Place::At(place) => Some((place, own)),
                    // Listed above, with no file read.
                    Place::Counted => None,
                    Place::Chain(chain, edition) => by_chain(chain, edition).map(|place| (place, own)),
                    Place::Tokens(chain, edition, at) => match by_chain(chain, edition) {
                        None => None,
                        Some(place) => {
                            const NOT_IN_ITEM: CliError = CliError::Text { what: "a body finding's token place is not in its item's text today (rule 71)" };
                            let item = text.get(place as usize..).ok_or(NOT_IN_ITEM)?;
                            let bytes = blaze_file_items::place_bytes(at, item, edition).map_err(|_| NOT_IN_ITEM)?;
                            let start = u32::try_from(bytes.start).map_err(|_| NOT_IN_ITEM)?;
                            let end = u32::try_from(bytes.end).map_err(|_| NOT_IN_ITEM)?;
                            Some((place, (start, end)))
                        }
                    },
                };
                into[first + i] = match place {
                    Some((place, range)) => placed_in(path, text, read, grammar.as_ref(), place, range, wanted[i].words, &prints[i])?,
                    None => unplaced(wanted[i].words, &prints[i], no_text, one),
                };
                held.saw(file, one, (file_at, line_at), text.len());
            }
            Ok(())
        });
        scratch.from.1 = scratch.one.used();
        match placed {
            Ok(done) => done?,
            Err(crate::work::Panicked) => return Err(scratch.stopped("placing", path)),
        }
    }
    Ok(())
}

/// One file's listed findings (`which`, their indices in `shown.listed`), printed: the file
/// read once into `scratch` and checked to be the text they were placed in, each finding drawn
/// in a frame emptied for it. `path` is `None` for the findings of no file.
pub fn file_findings<'f>(cx: &Cx, root: &Dir<ReadWrite>, path: Option<&str>, which: impl Iterator<Item = usize>, shown: &mut Shown<'f>, scratch: &mut Scratch, out: &Out) -> Result<(), CliError> {
    // `--fix`'s first check prints nothing: nothing is read for it either.
    let Some(path) = path.filter(|_| !shown.quiet) else {
        for at in which {
            let listed = shown.listed[at];
            finding(cx, &listed, &[], shown, scratch.one(), out)?;
        }
        return Ok(());
    };
    let file_at = scratch.next_file();
    let (file, one, held) = (&scratch.file, &mut scratch.one, &mut scratch.held);
    let drawn = crate::work::unwinding(|| -> Result<bool, CliError> {
        let mut bytes = Bytes::new_in(file, FILE_ARENA);
        root.read_all(cx, &Path::new(path)?, &mut bytes)?;
        let read = crate::fix::text_id(bytes.as_slice());
        for at in which {
            let listed = shown.listed[at];
            if listed.read != read {
                return Ok(false);
            }
            emptied(one);
            let line_at = one.used();
            finding(cx, &listed, bytes.as_slice(), shown, one, out)?;
            held.saw(file, one, (file_at, line_at), bytes.len());
        }
        Ok(true)
    });
    scratch.from.1 = scratch.one.used();
    match drawn {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => {
            let (path, cut) = path_shown(path);
            Err(CliError::Draw(DrawError::Changed { path, cut }))
        }
        Ok(Err(error)) => Err(error),
        Err(crate::work::Panicked) => Err(scratch.stopped("drawing", path)),
    }
}

/// A listed finding printed in the command's view: drawn for a person (its frame, or off a
/// terminal its line), or the agent's or JSON's plain lines. Its rule's words come from
/// blaze-report's table, its arguments the message's pieces between 0x1F bytes. `text` is its
/// file's, as it was listed in (unread for a finding of no file, or a quiet command).
pub fn finding<'f>(cx: &Cx, l: &Listed<'f>, text: &[u8], shown: &mut Shown<'f>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    // A task not run because an input it reads could not be made: the input's own finding is
    // the error, printed once; this one is kept by cause for one line (not_checked/tasks.rs),
    // its cause borrowed from the listed finding's words, never copied again.
    // A body not run had its types unchecked too: the closing line says "NOT checked: types".
    if l.area == Area::Cli && l.rule == crate::work::NOT_RUN {
        crate::not_checked::tasks::keep(&mut shown.not_run, l.about.kind, l.words)?;
        if l.about.kind == blaze_drive::Kind::BodyCheck {
            shown.types_unchecked += 1;
        }
        return Ok(());
    }
    if l.fix.is_some() {
        shown.fixes.push(l.id);
    }
    // A body whose types were not checked is no error of the program: it is a note, and it
    // keeps `check` from saying `ok` (main.rs `close`).
    let unchecked = l.area == Area::Cli && l.rule == crate::body_types::TYPES_NOT_CHECKED;
    // The move and borrow check (body_types.rs `BORROWS`, `BORROWS_NOT_CHECKED`): a refusal is
    // always a note; a finding is one until the check is trusted (`BORROWS_ARE_ERRORS`).
    let borrows_unchecked = l.area == Area::Cli && l.rule == crate::body_types::BORROWS_NOT_CHECKED;
    let borrows_note = borrows_unchecked || (l.area == Area::Cli && l.rule == crate::body_types::BORROWS && !crate::body_types::BORROWS_ARE_ERRORS);
    // A names warning (an include read outside its package inside the workspace, D111; a test
    // marked twice where rustc warns): never an error (main.rs `every_rule`).
    let outside = l.area == Area::Cli && crate::names::warning(l.rule);
    // A lint finding: the level the step's levels give its rule, now (rule 81). Advice is a
    // warning, and no step's is an error today (main.rs `every_rule`); an `Off` rule is not
    // drawn or counted.
    let lint = crate::lints::rule_of(l.rule).map(|_| crate::lints::level_of(shown.lints, l.rule));
    if lint == Some(blaze_lint::Level::Off) {
        return Ok(());
    }
    // A task that failed only because one it needs failed (work.rs `WAITED`): drawn only under
    // `--each` (main.rs `draw` counts it under its root), and then a note.
    let waited = l.area == Area::Cli && l.rule == crate::work::WAITED;
    // A task that failed on its own: a failure, counted apart from the code's errors.
    let root = l.area == Area::Cli && l.rule == crate::work::TASK_FAILED;
    let severity = if unchecked || borrows_note || waited {
        Severity::Note
    } else if lint == Some(blaze_lint::Level::Error) {
        Severity::Error
    } else if outside || lint.is_some() {
        Severity::Warning
    } else {
        Severity::Error
    };
    if unchecked {
        shown.types_unchecked += 1;
        crate::not_checked::items::keep(&mut shown.unchecked, l.path, l.words)?;
    }
    if borrows_unchecked {
        shown.borrows_unchecked += 1;
    }
    // A borrow note is neither drawn nor counted for a person: untrusted, census 3 found every
    // finding false; the closing line's "NOT checked: borrows" says the stage did not pass. The
    // agent's and JSON views list each with its code, range and why.
    if borrows_note && shown.view == View::Person {
        return Ok(());
    }
    // Every view sees an item or a body not checked as its cause's line (main.rs `draw`), not a
    // note an item: on a workspace they are thousands (tempo: 4,958). `--each` lists each.
    if shown.quiet || ((unchecked || borrows_unchecked) && !shown.each) {
        count(shown, severity, root, l.fix.is_some());
        return Ok(());
    }
    let message = core::str::from_utf8(l.words).map_err(|_| CliError::Text { what: "a finding's message is not UTF-8" })?;
    let Some(path) = l.path else {
        // A text-less crate's finding: no source to draw, its name is its place.
        count(shown, if waited { Severity::Note } else { Severity::Error }, root, false);
        let erased = blaze_own::Erased::new(frame);
        let about = format_in!(frame, "{}", l.about);
        let mut line = AString::new_in(erased.handle());
        match shown.view {
            View::Person => {
                let label = if waited { "note" } else { "error" };
                out.line(cx, format_args!("{}: {label}[{}]: {message}", l.about, l.code()))?;
                return Ok(());
            }
            View::Agent | View::Json => {
                let severity = if waited { Severity::Note } else { Severity::Error };
                let plain = Plain { id: l.id.as_str(), severity, code: l.code(), path: about.as_str(), text: None, start: 0, end: 0, message, fix: None };
                written(&mut line, &plain, shown.view)?;
            }
        }
        out.write_all(cx, line.as_bytes())?;
        return Ok(());
    };
    shown.file(path);
    let fix = l.fix.map(|change| (Edit { start: change.start, end: change.end, insert: change.insert }, change));
    draw(cx, shown, severity, l.area, l.rule, message, path, text, Some(l.item), (l.start, l.end), None, fix, l.id.as_str(), frame, out)?;
    // `draw` counted it as an error: a task that failed on its own is a failure instead.
    if root && shown.counts.errors > 0 {
        shown.counts.errors -= 1;
        shown.counts.failures += 1;
    }
    Ok(())
}

/// One finding of `severity` counted, not drawn; a task that failed on its own (`root`) as a
/// failure, apart from the code's errors.
fn count(shown: &mut Shown<'_>, severity: Severity, root: bool, fixable: bool) {
    if root {
        shown.counts.failures += 1;
    } else {
        shown.counts.add(severity, fixable);
    }
}

/// One plain finding into `line` in `view`.
fn written(line: &mut AString<'_>, plain: &Plain<'_>, view: View) -> Result<(), CliError> {
    let done = match view {
        View::Json => plain::json(line, plain),
        View::Agent | View::Person => plain::agent(line, plain),
    };
    done.map_err(|_| CliError::Text { what: "a fix's range is outside its file" })
}

/// A refusal of `blaze run`'s lowering drawn as its frame: `texts` the (path, text) of each
/// file given to the lowering, which its `file` indexes. After the last: the note of what the
/// command covered (main.rs, `left_out_note`), then the closing line.
pub fn refusal(cx: &Cx, refusal: &Refusal, texts: &[(&str, &str)], shown: &mut Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    let rule = refusal.rule as u16;
    // The level is the printer's, by the rule (rule 81): advice is a warning, the rest errors.
    let severity = if refusal.rule.is_advice() { Severity::Warning } else { Severity::Error };
    let Some((path, text)) = refusal.file.and_then(|index| texts.get(index as usize)) else {
        shown.counts.add(severity, false);
        out.line(cx, format_args!("{}[{}]: {}", severity.label(), refusal.rule.name(), &*refusal.message))?;
        return Ok(());
    };
    shown.file(path);
    let start = refusal.place.saturating_add(refusal.start);
    let end = refusal.place.saturating_add(refusal.end).max(start);
    let secondary = refusal.secondary.map(|(s, e)| (refusal.place.saturating_add(s), refusal.place.saturating_add(e)));
    draw(cx, shown, severity, Area::Ladder, rule, &refusal.message, path, text.as_bytes(), Some(refusal.place), (start, end), secondary, None, "", frame, out)
}

#[expect(clippy::too_many_arguments, reason = "the one drawing both kinds of finding share; each argument a different fact of it")]
fn draw(
    cx: &Cx,
    shown: &mut Shown<'_>,
    severity: Severity,
    area: Area,
    rule: u16,
    message: &str,
    path: &str,
    text: &[u8],
    item: Option<u32>,
    (start, end): (u32, u32),
    secondary: Option<(u32, u32)>,
    fix: Option<(Edit<'_>, Change<'_>)>,
    id: &str,
    frame: &Frame,
    out: &Out,
) -> Result<(), CliError> {
    // The frame carries no header of its own: erased into a handle here, for this finding.
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let mut args: [&str; 8] = [""; 8];
    let mut count = 0;
    for piece in message.split('\x1f') {
        if count == args.len() {
            return Err(CliError::Text { what: "a finding has more than eight arguments" });
        }
        args[count] = piece;
        count += 1;
    }
    let mut headline = AString::new_in(alloc);
    let mut label = AString::new_in(alloc);
    // A lint's message is whole, in blaze-lint's words (lints.rs `record`): its headline as it is.
    let name = match crate::lints::rule_of(rule) {
        Some(lint) => {
            headline.push_str(message);
            lint.name()
        }
        None => words::say(area, rule, &args[..count], &mut headline, &mut label).map_err(|error| match error {
            WordsError::NoWording { .. } => CliError::Text { what: "a finding's rule has no words in blaze_report::frame::words::WORDINGS" },
            WordsError::Arguments { .. } => CliError::Text { what: "a finding's arguments do not match its rule's words" },
        })?,
    };
    // Why the fix is the fix: a help's words (blaze-report `HELPS`).
    let mut reason = AString::new_in(alloc);
    if let Some((_, change)) = fix {
        words::help_words(&mut reason, change.reason, change.args()).map_err(|_| CliError::Text { what: "a fix's arguments do not match its help's words" })?;
    }
    if shown.view != View::Person {
        let plain = Plain { id, severity, code: name, path, text: Some(text), start, end, message: headline.as_str(), fix: fix.map(|(edit, _)| (edit, reason.as_str())) };
        let mut line = AString::new_in(alloc);
        written(&mut line, &plain, shown.view)?;
        shown.counts.add(severity, fix.is_some());
        out.write_all(cx, line.as_bytes())?;
        return Ok(());
    }
    let mut absolute = AString::new_in(alloc);
    absolute.push_str(shown.root);
    absolute.push_str("/");
    absolute.push_str(path);
    let others = [Mark { start: secondary.map_or(0, |it| it.0), end: secondary.map_or(0, |it| it.1), label: "" }];
    // The fix drawn as a preview, and applied by `blaze check --fix <id>` (D70).
    let edits = [fix.map_or(Edit { start, end, insert: "" }, |(edit, _)| edit)];
    let finding = Finding {
        severity,
        rule: name,
        message: headline.as_str(),
        file: 0,
        item,
        primary: Mark { start, end, label: label.as_str() },
        secondary: if secondary.is_some() { &others } else { &[] },
        fix: if fix.is_some() { &edits } else { &[] },
    };
    let sources = [Source { shown: path, absolute: absolute.as_str(), text }];
    let ids = [id];
    let input = Input { findings: core::slice::from_ref(&finding), sources: &sources, link: LINK_FILE, fixes: Fixes::Command, ids: &ids };
    let mut drawn = AString::new_in(alloc);
    // A blank line between frames.
    if shown.caps.form == Form::Frames && shown.counts.failures + shown.counts.errors + shown.counts.warnings + shown.counts.notes > 0 {
        drawn.push_str("\n");
    }
    render(&mut drawn, &input, shown.caps, alloc)
        .map_err(|_| CliError::Text { what: "a finding's range is outside its file" })?;
    // The reason under the frame, for a fix (the frame's preview shows the edit).
    if fix.is_some() {
        blaze_report::frame::help(&mut drawn, reason.as_str(), text, None, shown.caps).map_err(|_| CliError::Text { what: "a fix's range is outside its file" })?;
    }
    shown.counts.add(severity, fix.is_some());
    out.write_all(cx, drawn.as_bytes())?;
    Ok(())
}

/// A note about the command, not about a place in a file: one quiet line after the findings,
/// drawn by blaze-report (`frame::note`), in the findings' stream.
pub fn note(cx: &Cx, words: &str, shown: &Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    if shown.quiet {
        return Ok(());
    }
    let erased = blaze_own::Erased::new(frame);
    let mut line = AString::new_in(erased.handle());
    match shown.view {
        View::Json => plain::json_note(&mut line, words),
        View::Person | View::Agent => blaze_report::frame::note(&mut line, words, shown.caps),
    }
    out.write_all(cx, line.as_bytes())?;
    Ok(())
}

/// One note for each stage of `not` (main.rs `check_coverage`'s second list), in the order of
/// `work::CHECK_STAGES`, before the closing line: a "NOT checked" never stands without its
/// reason (rule 19), and names exactly the stage that did not run (the owner's rule: never a
/// pass printed for a stage that did not run, and never a fail for one that did). A stage this
/// build does not build says the build's reason; one it builds and did not run whole says what
/// left it so. Then, when bodies' borrow checks were refused and their notes were not drawn one
/// by one (engine_lines.rs `Class::Borrows`), one line counting them. `blaze check`'s.
pub(crate) fn stage_notes(cx: &Cx, not: &[&'static str], shown: &Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    for (stage, built, why_not_built) in crate::work::CHECK_STAGES {
        if !not.contains(&stage) {
            continue;
        }
        let why = match (built, stage) {
            (false, _) => why_not_built,
            (true, crate::work::NAMES_STAGE) => "the answer holds no crate's names (the notes above say why)",
            (true, crate::work::TYPES_STAGE) => "a body's types were not checked to the end (each such body is named in a note above)",
            // Also when a body's types stopped short: it is then not borrow checked, and the
            // types stage's own note above says so.
            (true, crate::work::BORROWS_STAGE) => "a body's move and borrow check did not run to the end (each such body is named in a note above)",
            (true, _) => "it did not run whole (the notes above say why)",
        };
        note(cx, format_in!(frame, "NOT checked: {stage}: {why}").as_str(), shown, frame, out)?;
    }
    let drawn = shown.each || crate::body_types::BORROWS_ARE_ERRORS;
    if shown.borrows_unchecked > 0 && !drawn {
        let n = shown.borrows_unchecked;
        let bodies = if n == 1 { "body" } else { "bodies" };
        note(cx, format_in!(frame, "{n} {bodies} type-checked whole had the move and borrow check refused (`--each` names each)").as_str(), shown, frame, out)?;
    }
    Ok(())
}

/// `--fix` refused (fix.rs `Refusal`): one line, `refused: <words>` (JSON: `{"refused": ..}`).
pub fn refused(cx: &Cx, words: &str, shown: &Shown<'_>, frame: &Frame, out: &Out) -> Result<(), CliError> {
    let erased = blaze_own::Erased::new(frame);
    let mut line = AString::new_in(erased.handle());
    match shown.view {
        View::Json => plain::json_refused(&mut line, words),
        View::Person | View::Agent => {
            line.push_str("refused: ");
            line.push_str(words);
            line.push_str("\n");
        }
    }
    out.write_all(cx, line.as_bytes())?;
    Ok(())
}

/// The command's last line: `1 error · 1 file checked · 1.1 ms`, or `ok · 14 files checked ·
/// 0.5 ms · nothing changed`. Not after a `run` whose program ran: its output is the output.
/// `tail`, when given, ends the same line: what the command typed did not do (`blaze test`'s
/// tests, not built).
#[expect(clippy::too_many_arguments, reason = "each argument a different fact of the one line")]
pub fn closing(
    cx: &Cx,
    shown: &Shown<'_>,
    checked: u32,
    nanos: u64,
    changed: Option<u32>,
    tail: Option<&str>,
    coverage: Option<Coverage<'_>>,
    frame: &Frame,
    out: &Out,
) -> Result<(), CliError> {
    if shown.quiet {
        return Ok(());
    }
    let erased = blaze_own::Erased::new(frame);
    let mut line = AString::new_in(erased.handle());
    // The fixes' ids, each once, in the order printed: `--fix` applies the one, or `all`.
    let mut ids: Vec<&str, &Frame> = Vec::with_capacity_in(shown.fixes.len(), frame);
    for id in shown.fixes.iter() {
        if !ids.contains(&id.as_str()) {
            ids.push(id.as_str());
        }
    }
    let fix_with = match ids.as_slice() {
        [] => "",
        [one] => one,
        _ => "all",
    };
    if shown.view != View::Person {
        // The plain views' end: no time, so the same tree gives the same bytes.
        let closing = PlainClosing { counts: shown.counts, fixes: &ids, coverage, command: "blaze check" };
        match shown.view {
            View::Json => plain::json_closing(&mut line, &closing),
            View::Agent | View::Person => plain::agent_closing(&mut line, &closing),
        }
        out.write_all(cx, line.as_bytes())?;
        return Ok(());
    }
    if shown.caps.form == Form::Frames && shown.counts.failures + shown.counts.errors + shown.counts.warnings + shown.counts.notes > 0 {
        line.push_str("\n");
    }
    closing_line(&mut line, &Closing { counts: shown.counts, checked, nanos, changed, fixes: Fixes::Command, coverage, fix_with }, shown.caps);
    if let Some(tail) = tail {
        if line.as_str().ends_with('\n') {
            line.pop();
        }
        line.push_str(if shown.caps.unicode { " · " } else { " - " });
        line.push_str(tail);
        line.push_str("\n");
    }
    out.write_all(cx, line.as_bytes())?;
    Ok(())
}

pub(crate) const PROFILE: &str = ".blaze/v2/profile.txt";

/// The profile and the command's phases, written in place and never synced: a crash, or a
/// reader during the write, sees a part of it; it is a report, read by a person, made again by
/// the next command. Not `publish`: bstd's unsynced publish renames through `Effect::Rename`,
/// which syncs the directory (`F_FULLFSYNC` on macOS, about 5 ms), the whole of a repeat
/// check's own time before this. One truncating open (its directory made only when missing),
/// one append, one close.
pub fn profile(cx: &Cx, root: &Dir<ReadWrite>, answer: &Answer, phases: &Phases, frame: &Frame) -> Result<(), CliError> {
    let text = format_in!(frame, "{}{}", answer.profile, phases);
    let file = root.create(cx, &Path::new(PROFILE)?)?;
    file.append(cx, text.as_bytes(), Durable::No)?;
    file.close(cx, Durable::No)?;
    Ok(())
}

/// The profile of a command that stopped without an answer (an error, a panic): why it
/// stopped, then the phases it got through and what its frame held, in the same file. Worked
/// in a frame of its own: the command's may be the full one that stopped it.
pub fn profile_failed(cx: &Cx, root: &Dir<ReadWrite>, error: &CliError, phases: &mut Phases, frame: &Frame) -> Result<(), CliError> {
    phases.mark_in("stopped", frame);
    phases.took("stopped", frame);
    let own = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).map_err(|_| CliError::Text { what: "the frame for a failed command's profile could not be reserved" })?;
    let text = format_in!(&own, "stopped      {error}\n{phases}");
    let file = root.create(cx, &Path::new(PROFILE)?)?;
    file.append(cx, text.as_bytes(), Durable::No)?;
    file.close(cx, Durable::No)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_drive::condition::OVER_CAP_RULE;
    use blaze_ladder::refusal::RefusalRule;
    use blaze_report::frame::words::{Area, wording};

    /// Every rule number this binary can print has a headline and a label in blaze-report's
    /// table: a new rule without words fails here, not in front of a person.
    #[test]
    fn every_rule_the_tree_emits_has_words() {
        let emitted = [
            (Area::Cli, crate::work::SYNTAX),
            (Area::Cli, crate::units::UNREAD),
            (Area::Cli, crate::units::TOOLCHAIN_NAMED),
            (Area::Cli, crate::units::TARGET_REFUSED),
            (Area::Cli, crate::work::MODULE_AT_BOTH),
            (Area::Cli, crate::names::UNRESOLVED),
            (Area::Cli, crate::names::failed_call::FAILED_CALL),
            (Area::Cli, crate::names::include_outside::INCLUDE_OUTSIDE_PACKAGE),
            (Area::Cli, crate::names::test_marks::TEST_REFUSED),
            (Area::Cli, crate::names::test_marks::TEST_DUPLICATED),
            (Area::Cli, crate::body_types::TYPES),
            (Area::Cli, crate::body_types::TYPES_NOT_CHECKED),
            (Area::Cli, crate::body_types::BORROWS),
            (Area::Cli, crate::body_types::BORROWS_NOT_CHECKED),
            (Area::Cli, crate::work::NOT_RUN),
            (Area::Drive, OVER_CAP_RULE),
            (Area::Drive, blaze_drive::request::MODULE_FILE_MISSING_RULE),
            (Area::Drive, blaze_drive::request::PATH_FILE_MISSING_RULE),
            (Area::Ladder, RefusalRule::Unsupported as u16),
            (Area::Ladder, RefusalRule::Syntax as u16),
            (Area::Ladder, RefusalRule::Program as u16),
            (Area::Ladder, RefusalRule::UnknownBlazeAttribute as u16),
        ];
        for (area, number) in emitted {
            assert!(wording(area, number).is_some(), "a rule blaze-cli prints has no words in WORDINGS: {area:?} {number}");
        }
    }
}
