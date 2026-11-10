//! What cargo prints of a build, from the daemon's responses to it: status lines and rustc's
//! diagnostics on stderr, the JSON messages on stdout, each unit's warnings and errors counted
//! as cargo's `JobQueue` and `on_stderr_line` count them (`core/compiler/job_queue/mod.rs`,
//! `core/compiler/mod.rs`).
//!
//! Per response, read where it lies in the daemon's frame ([`ResponseRef`]): its lines made in
//! one buffer the printer reuses, written when the caller [`Printer::flush`]es or when a line
//! for the other stream comes, so stdout and stderr keep the order their lines were made in. A
//! package is found by its dense index in the request ([`blaze_api::UnitStarted::package`]) in a
//! bitset, and a unit by its own ([`blaze_api::UnitOf::id`]) in a `u32` column: no hash and no
//! compare. A unit seen first keeps the two names its summary prints, copied once into one text
//! buffer. A diagnostic is deduplicated by a 64-bit hash of its text, as cargo's `DiagDedupe`
//! does, never a copy of it; an error with no span, a unit's own failure, is printed for each
//! unit it fails. Once the printer's tables have grown to the build's size, printing
//! a response allocates nothing.
//!
//! `--message-format=short` prints as `human` does (cargo's `on_stderr_line_inner` takes both
//! one way): the daemon renders each diagnostic in rustc's short form, `file:line:col: level:
//! message`, from the `--message-format` in `Cargo::args`, and cargo's own lines are the same.

use std::{
    fmt::Write as _,
    hash::BuildHasher as _,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_api::{
    Applicability, Artifact, Blocks, DiagnosticRef, FetchVerb, Level, Report, ResponseRef,
    Severity, TargetKind, Unchecked, UnitOfRef, Verb,
};
use blaze_hash::FixedState;
use std::alloc::Global;

use super::{json, say};

/// cargo's `MessageFormat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    #[default]
    Human,
    Short,
    Json {
        short: bool,
        ansi: bool,
        render_diagnostics: bool,
    },
}

impl Format {
    pub fn json(self) -> bool {
        matches!(self, Format::Json { .. })
    }

    /// Whether diagnostics are printed as rustc rendered them, on stderr.
    fn renders(self) -> bool {
        !matches!(self, Format::Json { render_diagnostics: false, .. })
    }
}

/// cargo's `ArgMatchesExt::message_format`: each `--message-format`, split at commas.
pub fn format(values: &[String]) -> Result<Format, String> {
    const TWO: &str = "cannot specify two kinds of `message-format` arguments";
    let json = Format::Json { short: false, ansi: false, render_diagnostics: false };
    let mut format = None;
    for value in values.iter().flat_map(|it| it.split(',')) {
        let value = value.to_ascii_lowercase();
        let only = |format: Option<Format>, it: Format| match format {
            // alloc: the error, once a request.
            Some(_) => Err(TWO.to_owned()),
            None => Ok(Some(it)),
        };
        let json_with = |format: &mut Option<Format>, set: fn(&mut bool, &mut bool, &mut bool)| {
            let mut it = format.unwrap_or(json);
            match &mut it {
                Format::Json { short, ansi, render_diagnostics } => {
                    set(short, ansi, render_diagnostics)
                }
                // alloc: the error, once a request.
                _ => return Err(TWO.to_owned()),
            }
            *format = Some(it);
            Ok(())
        };
        match value.as_str() {
            "json" => format = only(format, json)?,
            "human" => format = only(format, Format::Human)?,
            "short" => format = only(format, Format::Short)?,
            "json-render-diagnostics" => json_with(&mut format, |_, _, it| *it = true)?,
            "json-diagnostic-short" => json_with(&mut format, |it, _, _| *it = true)?,
            "json-diagnostic-rendered-ansi" => json_with(&mut format, |_, it, _| *it = true)?,
            // alloc: the error, once a request.
            other => return Err(format!("invalid message format specifier: `{other}`")),
        }
    }
    Ok(format.unwrap_or_default())
}

/// How the command was asked to speak.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Shown {
    pub format: Format,
    pub quiet: bool,
    pub verbose: u32,
    /// stderr is colored.
    pub color: bool,
    /// `BLAZE_STATS` is set: the request's run time by kind goes to stderr too.
    pub stats: bool,
    /// `check --fix`: the run makes the suggestions it prints, so no summary hints at
    /// `blaze check --fix`.
    pub fixing: bool,
    /// What stderr's terminal does beyond `color` (which wins): hyperlinks, its width, a
    /// progress bar, notifications ([`super::terminal`]). A pipe's by default.
    pub terminal: blaze_report::terminal::Terminal,
}

/// What one unit's diagnostics came to.
#[derive(Default)]
struct Counted {
    errors: u32,
    /// Every warning, its duplicates too (cargo's `errors_seen`, `warnings_seen`).
    warnings: u32,
    warned: say::Warned,
    /// Its summary was printed when it ended ([`ResponseRef::UnitEnded`]), as cargo prints it.
    ended: bool,
    /// Its failure was said as a report ([`ResponseRef::Report`]), which says all cargo's
    /// "could not compile" would: that line is left out.
    reported: bool,
}

const _: () = assert!(size_of::<Counted>() == 28);

/// What a unit's summary names it by, kept from the first response of it: its package's name
/// and its target's, each a range of [`Printer::names`], its target's kind and its mode.
struct Unit {
    package: Name,
    target: Name,
    kind: TargetKind,
    test: bool,
    /// The library's doc tests, named as such when they fail.
    doctest: bool,
    /// A path package, whose summary hints at `blaze check --fix`.
    local: bool,
}

const _: () = assert!(size_of::<Unit>() == 20);

/// A range of [`Printer::names`]: where it starts, and its length.
#[derive(Clone, Copy)]
struct Name(u32, u32);

pub(super) type ByHash<V, A> = blaze_hash::ByHash<u64, V, A>;
type HashedSet<A> = blaze_hash::ByHashSet<u64, A>;

/// What the build placed, once it said ([`ResponseRef::Built`]).
struct Built {
    artifacts: Vec<Artifact>,
    profile: String,
    members: u32,
}

/// No row for a unit's id yet.
const NONE: u32 = u32::MAX;

/// The daemon's indexes of units and packages are dense from 0, so one at or past this is its
/// mistake, refused by name before a table that long is made.
const IDS: u32 = 1 << 24;

/// What the tables are made for before the first response: a build of a few hundred units
/// fits, so none of them grows while it prints.
const PACKAGES: usize = 256;
const DIAGNOSTICS: usize = 1024;
/// Bytes of the units' names: a package's and a target's name, a few dozen bytes a unit.
const NAMES: usize = PACKAGES * 64;

/// The most bytes held for one stream before they are written, whether or not the caller has
/// flushed; the buffer holds this and one response's lines without growing.
const HELD: usize = 64 * 1024;

/// cargo's lines of a build, as its responses come.
pub struct Printer<'a, O: Write, E: Write> {
    shown: Shown,
    out: &'a mut O,
    err: &'a mut E,
    /// The lines made and not yet written, all for one stream (stdout when `to_out`).
    line: String,
    to_out: bool,
    sources: json::Sources,
    /// The packages a status line was printed for: a bit for each package's index.
    packages: Vec<u64>,
    /// Each unit's row by its id, [`NONE`] for one with no row yet.
    rows: Vec<u32>,
    /// Each unit with diagnostics, a row each in the order its first came: the counts read per
    /// response (hot), and what its summary names it by (cold).
    counted: Vec<Counted>,
    units: Vec<Unit>,
    /// The units' names, one after another.
    names: String,
    /// The hash of every diagnostic printed, as cargo's `DiagDedupe` keeps them.
    printed: HashedSet<Global>,
    built: Option<Built>,
    /// What the request's units came to, once its work report came ([`ResponseRef::Work`]):
    /// for `build-finished`.
    work: Option<json::Work>,
    /// The test units the build left unbuilt ([`blaze_api::Unchecked::tests`]), for the end of
    /// `test`'s run.
    unbuilt: Vec<blaze_api::Unbuilt>,
    /// The daemon numbered a unit or package past [`IDS`]: said, and the build fails.
    refused: bool,
    /// How many units the daemon said the build left unchecked or unbuilt behind a failure
    /// ([`ResponseRef::Unchecked`]); such a build failed whatever its `Done` says.
    left: usize,
    /// Where a report's paths are shown from: the checkout's root, and home as `~`
    /// ([`Printer::place`]).
    root: String,
    home: Option<String>,
    /// The reports of units ([`ResponseRef::Report`]) not yet printed, each with its unit's id:
    /// held until what each blocks is known ([`ResponseRef::Unchecked`]), or the build ends.
    held: Vec<(Option<u32>, Report)>,
    /// Every error report printed, kept for `blaze explain` once the build ends.
    failures: Vec<Report>,
    /// Where the failures are kept ([`super::observe::last_failure`]); `None` keeps none.
    remember: Option<Result<PathBuf, String>>,
    /// The build's live model as the last [`ResponseRef::Doing`] carried it, for its compact
    /// line off the live view ([`blaze_ui::board::Board::line`]): one when the phase, the first
    /// blocker or the memory's pressure changes, and otherwise every 10 s of the build.
    board: blaze_ui::board::Board,
}

impl<'a, O: Write, E: Write> Printer<'a, O, E> {
    /// `root` is the checkout's, which the files spans name are under.
    pub fn new(shown: Shown, root: &Path, out: &'a mut O, err: &'a mut E) -> Printer<'a, O, E> {
        Printer {
            shown,
            out,
            err,
            line: String::with_capacity(2 * HELD),
            to_out: false,
            sources: json::Sources::new(root),
            packages: Vec::with_capacity(PACKAGES.div_ceil(64) * 16),
            rows: Vec::with_capacity(PACKAGES * 4),
            counted: Vec::with_capacity(PACKAGES),
            units: Vec::with_capacity(PACKAGES),
            names: String::with_capacity(NAMES),
            printed: HashedSet::with_capacity_in(DIAGNOSTICS, Global),
            built: None,
            work: None,
            unbuilt: Vec::new(), // alloc: none until a test unit did not build
            refused: false,
            left: 0,
            root: root.to_string_lossy().into_owned(), // alloc: once a printer
            home: None,
            held: Vec::new(),     // alloc: none until a unit failed with a report
            // The compact line names no command and has no color: the board's are for its
            // drawn forms.
            board: blaze_ui::board::Board::new("blaze", blaze_ui::test_view::Paint::Plain),
            failures: Vec::new(), // alloc: as above
            remember: None,
        }
    }

    /// The home directory paths are shown under as `~`, and where the build's failures are
    /// kept for `blaze explain`: the command's. A printer not told shows paths whole but for
    /// the checkout's and keeps nothing.
    pub fn place(&mut self, home: Option<String>, remember: Result<PathBuf, String>) {
        self.home = home;
        self.remember = Some(remember);
    }

    /// How a report is rendered on stderr: the command's colour, and what its terminal does.
    fn caps(&self) -> blaze_report::Caps {
        let terminal = self.shown.terminal.caps;
        blaze_report::Caps { color: self.shown.color, links: terminal.links, width: terminal.width }
    }

    /// `report` on stderr, as `--message-format=json` a line on stdout too; an error kept for
    /// `blaze explain`.
    fn report(&mut self, report: Report) -> io::Result<()> {
        if self.shown.format.json() {
            self.to(true)?;
            self.line.push_str(&blaze_report::json_message(&report));
            self.line.push('\n');
        }
        self.to(false)?;
        let caps = self.caps();
        let roots = blaze_report::Roots { workspace: Some(&self.root), home: self.home.as_deref() };
        blaze_report::render_into(&mut self.line, &report, caps, &roots);
        if report.severity == Severity::Error {
            self.failures.push(report); // alloc: a failure's, once
        }
        Ok(())
    }

    /// After [`Printer::finish`]: why the build recomputed what it did, a report a root cause
    /// (`Rebuilt::summary`), each on stderr and, as `--message-format=json`, a line on stdout
    /// too; written. Nothing under `-q`.
    pub fn recomputed(&mut self, summary: Vec<Report>) -> io::Result<()> {
        if self.shown.quiet {
            return Ok(());
        }
        for report in summary {
            self.report(report)?;
        }
        self.flush()?;
        self.out.flush()?;
        self.err.flush()
    }

    /// Every report held, printed.
    fn reports(&mut self) -> io::Result<()> {
        for (_, report) in std::mem::take(&mut self.held) {
            self.report(report)?;
        }
        Ok(())
    }

    /// What each held report's unit blocks, from what the build left unchecked behind it: its
    /// packages (each unit named `package v1.2.3 (lib)`, its first word), in the plan's order,
    /// the last the one named.
    fn blocks(&mut self, unchecked: &Unchecked) {
        for behind in &unchecked.behind {
            let Some(id) = behind.id else { continue };
            let Some((_, report)) = self.held.iter_mut().find(|(unit, _)| *unit == Some(id)) else {
                continue;
            };
            let mut all: Vec<String> = Vec::new(); // alloc: a failure's, once
            for unit in &behind.units {
                let package = unit.split(' ').next().unwrap_or(unit);
                if !all.iter().any(|it| it == package) {
                    all.push(package.to_owned()); // alloc: as above
                }
            }
            let notable = all.last().cloned().into_iter().collect(); // clone: as above
            report.blocks =
                Some(Blocks { count: all.len() as u64, what: "packages".to_owned(), notable, all });
        }
    }

    /// Whether the daemon numbered a unit or package past any request's, which was said as an
    /// error: the build is then a failure whatever its Done said.
    pub fn refused(&self) -> bool {
        self.refused
    }

    /// How many units the daemon said the build did not check or build, behind a failure or
    /// failed themselves ([`blaze_api::Unchecked`], said only of a failed build): with any, the
    /// build is a failure whatever its `Done` said, so no command ends 0 with a requested unit
    /// unbuilt.
    pub fn left_unchecked(&self) -> usize {
        self.left
    }

    /// The daemon's `index` of a `what` is past [`IDS`]: said as an error, and the build fails.
    fn refuse(&mut self, what: &str, index: u32) -> io::Result<()> {
        self.to(false)?;
        self.refused = true;
        say::leveled_with(&mut self.line, say::ERROR, "error", self.shown.color, |line| {
            // Writing to a String cannot fail.
            let _ = write!(
                line,
                "the daemon numbered a {what} {index}, past the {IDS} a request's indexes stay under"
            );
        });
        Ok(())
    }

    /// Whether package `package` (its index) is seen here first; `None` once refused.
    fn first_of_package(&mut self, package: u32) -> io::Result<Option<bool>> {
        if package >= IDS {
            self.refuse("package", package)?;
            return Ok(None);
        }
        let (word, bit) = ((package / 64) as usize, 1u64 << (package % 64));
        if self.packages.len() <= word {
            self.packages.resize(word + 1, 0);
        }
        let first = self.packages[word] & bit == 0;
        self.packages[word] |= bit;
        Ok(Some(first))
    }

    /// Lines for stdout (`out`) or stderr come next: what is held for the other stream, or more
    /// than [`HELD`], is written first.
    fn to(&mut self, out: bool) -> io::Result<()> {
        if self.to_out != out || self.line.len() >= HELD {
            self.flush()?;
            self.to_out = out;
        }
        Ok(())
    }

    /// Writes the lines held, in one call.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.line.is_empty() {
            return Ok(());
        }
        match self.to_out {
            true => self.out.write_all(self.line.as_bytes())?,
            false => self.err.write_all(self.line.as_bytes())?,
        }
        self.line.clear();
        Ok(())
    }

    /// What the build placed, once it said, taken out of the printer.
    pub fn take_artifacts(&mut self) -> Vec<Artifact> {
        // alloc: none; an empty `Vec` holds no memory.
        self.built.as_mut().map_or_else(Vec::new, |built| std::mem::take(&mut built.artifacts))
    }

    /// The test units the build left unbuilt, taken out of the printer.
    pub fn take_unbuilt(&mut self) -> Vec<blaze_api::Unbuilt> {
        std::mem::take(&mut self.unbuilt)
    }

    /// The workspace's member count the build said; 1 before it did.
    pub fn members(&self) -> u32 {
        self.built.as_ref().map_or(1, |built| built.members)
    }

    /// stderr, written to directly once the lines held are flushed.
    pub fn err(&mut self) -> &mut E {
        debug_assert!(self.line.is_empty(), "lines held would come after what is written now");
        self.err
    }

    /// stdout and stderr, for what follows the build.
    pub fn streams(&mut self) -> (&mut O, &mut E) {
        debug_assert!(self.line.is_empty(), "lines held would come after what is written now");
        (self.out, self.err)
    }

    pub fn shown(&self) -> Shown {
        self.shown
    }

    /// One of cargo's status lines that no response is (`doc`'s `Documenting`), held as the
    /// others are; none under `-q`.
    pub fn status(&mut self, word: &str, what: &str) -> io::Result<()> {
        if self.shown.quiet {
            return Ok(());
        }
        self.to(false)?;
        say::status_word(&mut self.line, word, self.shown.color);
        self.line.push_str(what);
        self.line.push('\n');
        Ok(())
    }

    /// One response of the build, read where it lies, its lines held until [`Printer::flush`].
    pub fn on(&mut self, response: ResponseRef<'_>) -> io::Result<()> {
        match response {
            ResponseRef::Unit(unit) => {
                let word = match unit.verb {
                    Verb::Compiling => "Compiling",
                    Verb::Checking => "Checking",
                    Verb::Fresh => "Fresh",
                };
                // A fresh unit names its package only where it is shown: cargo marks one fresh
                // only as the package's last unit, and the units of a package come in no order
                // the client knows.
                let shows = match unit.verb {
                    Verb::Fresh => self.shown.verbose > 0,
                    Verb::Compiling | Verb::Checking => true,
                };
                if !shows || self.first_of_package(unit.package)? != Some(true) {
                    return Ok(());
                }
                if !self.shown.quiet {
                    self.to(false)?;
                    say::status_word(&mut self.line, word, self.shown.color);
                    self.line.push_str(unit.name);
                    self.line.push_str(" v");
                    self.line.push_str(unit.version);
                    if let Some(source) = unit.source {
                        self.line.push(' ');
                        self.line.push_str(source);
                    }
                    self.line.push('\n');
                }
            }
            // cargo's `Updating crates.io index`, `Downloaded serde v1.0.219`.
            ResponseRef::Fetched { verb, what } if !self.shown.quiet => {
                let word = match verb {
                    FetchVerb::Updating => "Updating",
                    FetchVerb::Downloaded => "Downloaded",
                };
                self.to(false)?;
                say::status_word(&mut self.line, word, self.shown.color);
                self.line.push_str(what);
                self.line.push('\n');
            }
            ResponseRef::Diagnostic(diagnostic) => self.diagnostic(diagnostic)?,
            // `[15m14s] compiling 488/570 · 1/16 busy · mem 20.1/24.0 GB · on codegen core (…)`:
            // the build's compact line when one is due, never under `-q` or JSON.
            ResponseRef::Doing(doing) if !self.shown.quiet && !self.shown.format.json() => {
                self.board.take(doing.now);
                self.to(false)?;
                if self.board.line(&mut self.line) {
                    self.line.push('\n');
                }
            }
            ResponseRef::BuildScript(ran) if self.shown.format.json() => {
                self.to(true)?;
                json::build_script(&mut self.line, ran);
                self.line.push('\n');
            }
            ResponseRef::Built { artifacts, profile, members } => {
                if self.shown.format.json() {
                    self.to(true)?;
                    for artifact in &artifacts {
                        json::artifact(&mut self.line, artifact);
                        self.line.push('\n');
                    }
                }
                self.built = Some(Built { artifacts, profile, members });
            }
            // cargo prints a unit's "generated N warnings" or "could not compile" as it ends.
            ResponseRef::UnitEnded(unit) => {
                let Some(at) = self.unit(unit)? else { return Ok(()) };
                if self.counted[at].ended {
                    return Ok(());
                }
                self.counted[at].ended = true;
                self.to(false)?;
                summary(
                    &mut self.line,
                    &self.units[at],
                    &self.names,
                    &self.counted[at],
                    self.shown,
                );
            }
            // What a failed build left unchecked: the tree to stderr in every format, so a
            // person sees how much the errors above do not cover; under JSON one line an agent
            // reads too.
            ResponseRef::Unchecked(unchecked) => {
                // At least one: the daemon says this only once a unit failed.
                self.left += (unchecked.behind.iter().map(|it| it.units.len()).sum::<usize>()
                    + unchecked.tests.len()
                    + unchecked.open.len())
                .max(1);
                // The failures it is behind, each with what it blocks, first.
                self.blocks(&unchecked);
                self.reports()?;
                if self.shown.format.json() {
                    self.to(true)?;
                    self.line.push_str(&blaze_ui::api::unchecked_json(&unchecked));
                }
                self.to(false)?;
                blaze_ui::api::unchecked_lines(&mut self.line, &unchecked, self.shown.color);
                self.unbuilt.extend(unchecked.tests.iter().cloned()); // clone: a failed build's, once
            }
            // blaze's own, for the proof harness: what the request did, then what became of each
            // dependency crate's check.
            // How each image was linked goes to stderr in every format: a link that was not
            // spliced is never silent.
            ResponseRef::Work(worked) => {
                self.work = Some(json::Work::of(&worked));
                if self.shown.format.json() {
                    self.to(true)?;
                    // alloc: two texts a request, at its end.
                    self.line.push_str(&blaze_ui::api::work_json(&worked));
                    self.line.push_str(&blaze_ui::api::served_json(&worked.served));
                }
                self.to(false)?;
                blaze_ui::api::link_lines(&mut self.line, &worked.links);
                if self.shown.stats {
                    blaze_ui::api::timeline_line(&mut self.line, &worked.steps_us);
                }
            }
            // A unit's failure, said by blaze (a build script's, a refused directive): its
            // unit's error, said once what it blocks is known.
            ResponseRef::Report { unit, report } => {
                if report.severity == Severity::Error
                    && let Some(unit) = unit
                    && let Some(at) = self.unit(unit)?
                {
                    self.counted[at].errors += 1;
                    self.counted[at].reported = true;
                }
                self.held.push((unit.map(|it| it.id), report)); // alloc: a failure's, once
            }
            ResponseRef::Error(error) => self.report(error.into_report())?,
            _ => {}
        }
        Ok(())
    }

    fn diagnostic(&mut self, diagnostic: DiagnosticRef<'_>) -> io::Result<()> {
        // rustc's own summary, which cargo skips in every format and replaces with its own (it
        // has no span and no code, so the text is compared only then); and an empty text, the
        // note naming codes to explain, which rustc doesn't write under short.
        if diagnostic.rendered.is_empty()
            || (diagnostic.spans.is_empty()
                && diagnostic.code.is_none()
                && summarizes(diagnostic.message))
        {
            return Ok(());
        }
        let renders = self.shown.format.renders();
        if let Format::Json { ansi, render_diagnostics: false, .. } = self.shown.format
            && let Some(unit) = diagnostic.unit
        {
            self.to(true)?;
            json::compiler_message(&mut self.line, unit, diagnostic, ansi, &mut self.sources);
            self.line.push('\n');
        }
        // A diagnostic of no unit (a warning of the whole job) has no `compiler-message` to
        // carry it, so its text goes to stderr in every format, as cargo writes its own warnings
        // under `--message-format=json`; where the format renders, it is written there once.
        let writes = renders || diagnostic.unit.is_none();
        // cargo takes one newline off the end, then writes the text and a newline.
        let rendered = diagnostic.rendered.strip_suffix('\n').unwrap_or(diagnostic.rendered);
        // An error with no span is a unit's own failure (a refusal of blaze's, a build script
        // that failed), said once a unit: deduplicated, a second unit failing with the same
        // words printed only its "could not compile" (selfbuild2: psm's and llvm-sys's build
        // scripts). Spanned diagnostics are deduplicated as cargo's `DiagDedupe` does.
        let own_failure = diagnostic.level == Level::Error && diagnostic.spans.is_empty();
        let printed = match writes {
            true => own_failure || self.printed.insert(FixedState.hash_one(rendered)),
            false => true,
        };
        if writes && printed {
            self.to(false)?;
            self.line.push_str(rendered);
            self.line.push('\n');
        }
        let Some(unit) = diagnostic.unit else { return Ok(()) };
        let fixable = fixable(diagnostic);
        let Some(at) = self.unit(unit)? else { return Ok(()) };
        let counted = &mut self.counted[at];
        match diagnostic.level {
            Level::Warning => {
                counted.warnings += 1;
                if renders {
                    counted.warned.total += 1;
                    match printed {
                        false => counted.warned.duplicates += 1,
                        true if fixable => {
                            counted.warned.fixable = counted.warned.fixable.map(|it| it + 1);
                        }
                        true => {}
                    }
                }
            }
            Level::Error => {
                counted.errors += 1;
                counted.warned.fixable = None;
            }
            Level::Note | Level::Help | Level::FailureNote => {}
        }
        Ok(())
    }

    /// The row of `unit`, found by its id; made the first time it comes, with the names its
    /// summary prints copied out of the frame. `None` once refused.
    fn unit(&mut self, unit: UnitOfRef<'_>) -> io::Result<Option<usize>> {
        if unit.id >= IDS {
            self.refuse("unit", unit.id)?;
            return Ok(None);
        }
        let id = unit.id as usize;
        if self.rows.len() <= id {
            self.rows.resize(id + 1, NONE);
        }
        if self.rows[id] != NONE {
            return Ok(Some(self.rows[id] as usize));
        }
        let row = self.counted.len();
        self.rows[id] = row as u32;
        let package = name(&mut self.names, say::package_name(unit.package_id));
        let target = name(&mut self.names, unit.target.name);
        let local = unit.package_id.starts_with("path+");
        self.units.push(Unit {
            package,
            target,
            kind: unit.target.kind,
            test: unit.test,
            doctest: unit.doctest,
            local,
        });
        let warned = say::Warned { fixable: Some(0), ..say::Warned::default() };
        self.counted.push(Counted { warned, ..Counted::default() });
        Ok(Some(row))
    }

    /// The build is over (`ok` is its `Done`'s), `took` after it began: each unit's summary,
    /// `Finished` and `build-finished`, all written.
    pub fn finish(&mut self, ok: bool, took: Duration) -> io::Result<()> {
        let color = self.shown.color;
        self.reports()?;
        self.to(false)?;
        for (unit, counted) in self.units.iter().zip(&self.counted) {
            if !counted.ended {
                summary(&mut self.line, unit, &self.names, counted, self.shown);
            }
        }
        if ok
            && !self.shown.quiet
            && let Some(built) = &self.built
        {
            say::status_word(&mut self.line, "Finished", color);
            say::finished_into(&mut self.line, &built.profile, took);
            self.line.push('\n');
        }
        // A slow build, done or not, points at what says why (`blaze slow`, docs/profiling.md).
        if took >= SLOW_BUILD && !self.shown.quiet && !self.shown.format.json() {
            let what = format!(
                "this build took {}: `blaze slow` says what made it slow and what to fix",
                say::elapsed(took)
            );
            say::note_into(&mut self.line, &what, color);
        }
        // Kept for `blaze explain <package>`, which lists all a failure blocks.
        if let (Some(remember), false) = (&self.remember, self.failures.is_empty()) {
            let kept = remember
                .as_ref()
                .map_err(String::clone)
                .and_then(|file| super::observe::remember(file, &self.failures));
            if let Err(why) = kept {
                self.to(false)?;
                say::warning_into(
                    &mut self.line,
                    &format!("`blaze explain` won't know this build's failures: {why}"),
                    color,
                );
            }
            self.failures.clear();
        }
        if self.shown.format.json() {
            self.to(true)?;
            let finished = json::Finished {
                success: ok,
                profile: self.built.as_ref().and_then(|it| json::profile_name(&it.profile)),
                served: json::Served::HERE,
                took_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                work: self.work,
            };
            json::build_finished(&mut self.line, &finished);
            self.line.push('\n');
        }
        self.flush()?;
        self.out.flush()?;
        self.err.flush()
    }
}

/// A build at least this long ends with a pointer to `blaze slow`.
const SLOW_BUILD: Duration = Duration::from_secs(30);

/// rustc's closing lines, which cargo replaces with its own summary.
fn summarizes(message: &str) -> bool {
    message.starts_with("aborting due to")
        || message.ends_with(" warning emitted")
        || message.ends_with(" warnings emitted")
}

/// A unit's "generated N warnings" (`JobQueue::report_warning_count` as the unit finishes, a
/// failed one too, silent under `-q`), then its "could not compile" (the unit's error, which
/// `-q` does not silence), into `line`.
fn summary(line: &mut String, unit: &Unit, names: &str, counted: &Counted, shown: Shown) {
    let package = named(names, unit.package);
    let target = say::Named { kind: unit.kind, name: named(names, unit.target) };
    if !shown.quiet && counted.warned.total > 0 {
        say::leveled_with(line, say::WARN, "warning", shown.color, |line| {
            say::generated_into(
                line,
                package,
                target,
                unit.test,
                unit.local && !shown.fixing,
                counted.warned,
            );
        });
    }
    if counted.errors > 0 && !counted.reported {
        say::leveled_with(line, say::ERROR, "error", shown.color, |line| match unit.doctest {
            true => say::doctests_could_not_compile_into(
                line,
                package,
                counted.errors,
                counted.warnings,
            ),
            false => say::could_not_compile_into(
                line,
                package,
                target,
                unit.test,
                counted.errors,
                counted.warnings,
            ),
        });
    }
}

/// `text` kept at the end of `names`.
fn name(names: &mut String, text: &str) -> Name {
    let at = names.len() as u32;
    names.push_str(text);
    Name(at, text.len() as u32)
}

/// The text `name` is of `names`.
fn named(names: &str, Name(at, len): Name) -> &str {
    &names[at as usize..(at + len) as usize]
}

/// cargo's `machine_applicable`: a child of the diagnostic suggests a fix rustfix may apply.
fn fixable(diagnostic: DiagnosticRef<'_>) -> bool {
    diagnostic.children.iter().any(|child| {
        child.spans.iter().any(|span| span.applicability == Some(Applicability::MachineApplicable))
    })
}

#[cfg(test)]
mod tests;
