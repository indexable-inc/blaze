//! What a build says, from the daemon's responses to it: the JSON messages on stdout, as
//! cargo writes them, and on stderr the screen's events (docs/terminal.md): each response
//! becomes what the screen is told ([`blaze_screen::Event`]: the build as it runs now, a unit
//! started, a diagnostic's drawn lines, the build's end and its why), and the screen's one
//! core decides what a terminal or a pipe is shown of them. Nothing of a build's progress is
//! made into text here.
//!
//! Per response, read where it lies in the daemon's frame ([`ResponseRef`]): its lines made in
//! one buffer the printer reuses, handed on when the caller [`Printer::flush`]es or when a line
//! for the other stream comes, so stdout and stderr keep the order their lines were made in. A
//! package is found by its dense index in the request ([`blaze_api::UnitStarted::package`]) in a
//! bitset. A diagnostic is deduplicated by a 64-bit hash of its text, as cargo's `DiagDedupe`
//! does, never a copy of it; an error with no span, a unit's own failure, is said for each
//! unit it fails.
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
    Artifact, Blocks, DiagnosticRef, Level, LinkHow, Linked, Report, ResponseRef, Severity,
    TargetKind, Unchecked, UnitOfRef, Verb,
};
use blaze_hash::FixedState;
use blaze_screen::{Event, Link, Why};
use std::alloc::Global;

use super::{
    json, say,
    show::{Sink, Stay},
};

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
    /// stderr is a terminal whose region is redrawn (docs/terminal.md): a note that only
    /// says how the build is going is the region's to show, and stays out of the scrollback
    /// unless `-v` asks for it.
    pub live: bool,
}

pub(super) type ByHash<V, A> = blaze_hash::ByHash<u64, V, A>;
type HashedSet<A> = blaze_hash::ByHashSet<u64, A>;

/// What the build placed, once it said ([`ResponseRef::Built`]).
struct Built {
    artifacts: Vec<Artifact>,
    profile: String,
    members: u32,
}

/// The daemon's indexes of units and packages are dense from 0, so one at or past this is its
/// mistake, refused by name before a table that long is made.
const IDS: u32 = 1 << 24;

/// What the tables are made for before the first response: a build of a few hundred units
/// fits, so none of them grows while it prints.
const PACKAGES: usize = 256;
const DIAGNOSTICS: usize = 1024;

/// The most bytes held for one stream before they are written, whether or not the caller has
/// flushed; the buffer holds this and one response's lines without growing.
const HELD: usize = 64 * 1024;

/// What a build says, as its responses come: stdout's JSON messages written, stderr's handed
/// to the screen (`S`).
pub struct Printer<'a, O: Write, S: Sink> {
    shown: Shown,
    out: &'a mut O,
    err: &'a mut S,
    /// The lines made and not yet written, all for one stream (stdout when `to_out`).
    line: String,
    to_out: bool,
    sources: json::Sources,
    /// The packages a status line was printed for: a bit for each package's index.
    packages: Vec<u64>,
    /// Warnings said that a fix exists for (`check --fix` makes them); 0 while fixing.
    fixable: u32,
    /// The first unit that said an error, as the build's last line names it, and its
    /// package: what `blaze explain` is asked of.
    failed: Option<String>,
    /// The hash of every diagnostic printed, as cargo's `DiagDedupe` keeps them.
    printed: HashedSet<Global>,
    built: Option<Built>,
    /// What the request's units came to, once its work report came ([`ResponseRef::Work`]):
    /// for `build-finished`.
    work: Option<json::Work>,
    /// Each image the build linked ([`blaze_api::Worked::links`]): for the build's last line.
    links: Vec<Linked>,
    /// The build's goal and how much of what it linked is optimized, once its work report
    /// came ([`blaze_api::Worked::tiers`]): for the `Finished` line.
    tiers: Option<blaze_api::Tiers>,
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
}

impl<'a, O: Write, S: Sink> Printer<'a, O, S> {
    /// `root` is the checkout's, which the files spans name are under.
    pub fn new(shown: Shown, root: &Path, out: &'a mut O, err: &'a mut S) -> Printer<'a, O, S> {
        Printer {
            shown,
            out,
            err,
            line: String::with_capacity(2 * HELD),
            to_out: false,
            sources: json::Sources::new(root),
            packages: Vec::with_capacity(PACKAGES.div_ceil(64) * 16),
            failed: None,
            fixable: 0,
            printed: HashedSet::with_capacity_in(DIAGNOSTICS, Global),
            built: None,
            work: None,
            links: Vec::new(), // alloc: none until an image is linked
            tiers: None,
            unbuilt: Vec::new(), // alloc: none until a test unit did not build
            refused: false,
            left: 0,
            root: root.to_string_lossy().into_owned(), // alloc: once a printer
            home: None,
            held: Vec::new(),     // alloc: none until a unit failed with a report
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
        if report.severity == Severity::Note && self.shown.live && self.shown.verbose == 0 {
            return Ok(());
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
        self.err.take(Event::Diagnosed { error: true, unit: None });
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
            false => {
                // alloc: a burst of lines that stay, handed to the screen
                self.err.take(Event::Said(std::mem::take(&mut self.line)));
                self.line.reserve(HELD);
                return self.err.draw();
            }
        }
        self.line.clear();
        Ok(())
    }

    /// What is held handed on, and the screen's frame that is due written: at the end of each
    /// burst of responses.
    pub fn draw(&mut self) -> io::Result<()> {
        self.flush()?;
        self.err.draw()
    }

    /// Nothing live is left on the terminal after this: what follows (the program a `run`
    /// becomes, test executables that write for themselves) owns it.
    pub fn clear(&mut self) -> io::Result<()> {
        self.flush()?;
        self.err.take(Event::Clear);
        self.err.settle()
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

    /// stderr, for whole lines that stay, once the lines held are flushed.
    pub fn err(&mut self) -> Stay<'_, S> {
        debug_assert!(self.line.is_empty(), "lines held would come after what is written now");
        Stay::new(self.err)
    }

    /// stdout and stderr, for what follows the build.
    pub fn streams(&mut self) -> (&mut O, Stay<'_, S>) {
        debug_assert!(self.line.is_empty(), "lines held would come after what is written now");
        (self.out, Stay::new(self.err))
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
            // A unit started: the screen's, which names it only under `-v` (a region shows
            // what is under way from the build's model, never a line a unit).
            ResponseRef::Unit(unit) => {
                if self.first_of_package(unit.package)? == Some(true) {
                    self.flush()?;
                    // alloc: a name a package, once
                    let name = format!("{} v{}", unit.name, unit.version);
                    self.err.take(Event::Unit { name, fresh: unit.verb == Verb::Fresh });
                }
            }
            // `Updating crates.io index`, `Downloaded serde v1.0.219`: under `-v` a line each;
            // otherwise the build's model carries each download while it is in flight.
            ResponseRef::Fetched { verb, what } if self.shown.verbose > 0 && !self.shown.quiet => {
                self.to(false)?;
                // Writing to a String cannot fail.
                let _ = writeln!(self.line, "  {} {what}", format!("{verb:?}").to_lowercase());
            }
            ResponseRef::Diagnostic(diagnostic) => self.diagnostic(diagnostic)?,
            // The build as it runs now: the screen's model of it.
            ResponseRef::Doing(doing) => {
                self.flush()?;
                self.err.take(Event::Now(Box::new(doing.now))); // alloc: the model, twice a second
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
                // The tree of what was not checked: a pipe's and `-v`'s, by name; a terminal's
                // one line of the failure counts it, and `blaze explain` names it.
                if !self.shown.live || self.shown.verbose > 0 {
                    self.to(false)?;
                    blaze_ui::api::unchecked_lines(&mut self.line, &unchecked, self.shown.color);
                }
                self.unbuilt.extend(unchecked.tests.iter().cloned()); // clone: a failed build's, once
            }
            // blaze's own, for the proof harness: what the request did, then what became of each
            // dependency crate's check.
            // How each image was linked is the build's last line's: a link that was not
            // spliced is never silent there.
            ResponseRef::Work(worked) => {
                self.work = Some(json::Work::of(&worked));
                self.tiers = worked.tiers;
                if self.shown.format.json() {
                    self.to(true)?;
                    // alloc: two texts a request, at its end.
                    self.line.push_str(&blaze_ui::api::work_json(&worked));
                    self.line.push_str(&blaze_ui::api::served_json(&worked.served));
                }
                if self.shown.stats {
                    self.to(false)?;
                    blaze_ui::api::timeline_line(&mut self.line, &worked.steps_us);
                }
                self.links = worked.links;
            }
            // A unit's failure, said by blaze (a build script's, a refused directive): its
            // unit's error, said once what it blocks is known.
            ResponseRef::Report { unit, report } => {
                if report.severity == Severity::Error {
                    let named = match unit {
                        Some(unit) => self.unit(unit, true)?,
                        None => None,
                    };
                    self.flush()?;
                    self.err.take(Event::Diagnosed { error: true, unit: named });
                }
                self.held.push((unit.map(|it| it.id), report)); // alloc: a failure's, once
            }
            // An error that ends the request: counted, so the build's last line says it failed
            // of an error.
            ResponseRef::Error(error) => {
                self.report(error.into_report())?;
                self.flush()?;
                self.err.take(Event::Diagnosed { error: true, unit: None });
            }
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
        // A pause of the whole build and its end (blaze-graph's `pressure`: memory is short) is
        // what the build waits on now: on a terminal the region's to show while it lasts, never
        // a line a pause in the scrollback.
        if diagnostic.unit.is_none()
            && diagnostic.level == Level::Note
            && self.shown.live
            && self.shown.verbose == 0
        {
            let waits = match diagnostic.message.strip_prefix("build paused: ") {
                Some(why) => Some(Some(why.to_owned())), // alloc: a pause, once
                None if diagnostic.message.starts_with("build resumed") => Some(None),
                None => None,
            };
            if let Some(waits) = waits {
                self.flush()?;
                self.err.take(Event::Waits(waits));
                return Ok(());
            }
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
        let error = match diagnostic.level {
            Level::Error => true,
            Level::Warning => false,
            Level::Note | Level::Help | Level::FailureNote => return Ok(()),
        };
        // A warning said before is not counted again.
        if !error && !printed {
            return Ok(());
        }
        if !error && !self.shown.fixing && fixable(diagnostic) {
            self.fixable += 1;
        }
        let named = match diagnostic.unit {
            Some(unit) => self.unit(unit, error)?,
            None => None,
        };
        self.flush()?;
        self.err.take(Event::Diagnosed { error, unit: named });
        Ok(())
    }

    /// `unit` as the build's last line names it (`hello (bin)`, `calc`, `calc (test it)`);
    /// the first that fails is kept. `None` once refused.
    fn unit(&mut self, unit: UnitOfRef<'_>, error: bool) -> io::Result<Option<String>> {
        if unit.id >= IDS {
            self.refuse("unit", unit.id)?;
            return Ok(None);
        }
        let package = say::package_name(unit.package_id);
        // alloc: a name a diagnostic's unit, on a failing or warning build
        let named = match (unit.target.kind, unit.test) {
            (_, _) if unit.doctest => format!("{package} (doc tests)"),
            (TargetKind::Lib, false) => package.to_owned(),
            (TargetKind::Lib, true) => format!("{package} (lib test)"),
            (TargetKind::Bin, false) if unit.target.name == package => format!("{package} (bin)"),
            (TargetKind::Bin, _) => format!("{package} (bin {})", unit.target.name),
            (TargetKind::Test, _) => format!("{package} (test {})", unit.target.name),
            (TargetKind::Bench, _) => format!("{package} (bench {})", unit.target.name),
            (TargetKind::Example, _) => format!("{package} (example {})", unit.target.name),
            (TargetKind::CustomBuild, _) => format!("{package} (build script)"),
        };
        if error && self.failed.is_none() {
            self.failed = Some(package.to_owned()); // alloc: once a build
        }
        Ok(Some(named))
    }

    /// The build is over (`ok` is its `Done`'s), `took` after it began: the reports held,
    /// then the build's end as the screen takes it ([`Event::Built`], whose one line the
    /// screen decides), `why` with it when the build's why is known (a report a root cause,
    /// `Rebuilt::summary`; `-v` says each whole), and `build-finished`, all written. `tests`:
    /// tests run next in this command's live view, which keeps the region.
    pub fn finish(
        &mut self,
        ok: bool,
        took: Duration,
        why: Option<Vec<Report>>,
        tests: bool,
    ) -> io::Result<()> {
        let color = self.shown.color;
        self.reports()?;
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
        self.flush()?;
        let work = self.work;
        let next = match ok {
            // What says more of the failure: all it blocks, by its package.
            false => self.failed.as_ref().map(|package| format!("blaze explain {package}")), // alloc: once a build
            // A slow build points at what says why (`blaze slow`, docs/profiling.md).
            true if took >= SLOW_BUILD => Some("blaze slow".to_owned()), // alloc: as above
            true => None,
        };
        let program = self
            .built
            .as_ref()
            .and_then(|built| blaze_thin::answer::placed(&built.artifacts))
            .map(str::to_owned); // alloc: a path, once a build
        let built = blaze_screen::Built {
            size_bytes: program.as_deref().and_then(blaze_thin::answer::on_disk),
            ok,
            compiled: work.map_or(0, |it| it.built),
            cached: work.map_or(0, |it| it.replayed),
            tier: self.tiers.as_ref().map(blaze_api::Tiers::status),
            // alloc: a link an image, once a build
            links: std::mem::take(&mut self.links)
                .into_iter()
                .map(|it| Link {
                    whole: matches!(it.how, LinkHow::Full { .. }),
                    how: it.how.to_string(),
                    path: it.path,
                })
                .collect(),
            unbuilt: u32::try_from(self.left).unwrap_or(u32::MAX),
            fixable: self.fixable,
            program,
            next,
            why_follows: why.is_some(),
            tests_follow: tests,
        };
        self.err.take(Event::Built(built));
        if let Some(why) = why {
            // alloc: a cause each, once a build
            self.err.take(Event::Why(why.iter().map(why_of).collect()));
            // The reports whole, as `blaze why` prints them.
            if self.shown.verbose > 0 && !self.shown.quiet {
                for report in why {
                    self.report(report)?;
                }
                self.flush()?;
            }
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
        self.err.draw()
    }
}

/// One cause of a build's why (`Rebuilt::summary`'s report of it), as the screen takes it:
/// its headline as the report has it, each part with its role; the cause's own words
/// (`src/main.rs changed`: what follows `because` in the headline); the functions compiled
/// again for it (its `codegen` facts); and whether it is work that should not have been done
/// (said as a warning).
fn why_of(report: &Report) -> Why {
    use blaze_report::Span;
    let mut cause = blaze_api::Text::new();
    let mut found = false;
    for span in &report.headline.0 {
        match span {
            Span::Plain(words) if !found => {
                if let Some((_, after)) = words.split_once(" because ") {
                    found = true;
                    if !after.is_empty() {
                        cause.push(Span::Plain(after.to_owned())); // alloc: once a cause
                    }
                }
            }
            span if found => cause.push(span.clone()), // clone: as above
            _ => {}
        }
    }
    let mut functions: Vec<String> = Vec::new(); // alloc: as above
    for fact in &report.facts {
        let text = fact.text.flat();
        let Some((step, names)) = text.split_once(": ") else { continue };
        if !step.starts_with("codegen") {
            continue;
        }
        for name in names.split(", ") {
            let name = name.trim_matches('`');
            // `main` beside `hello::main` is the program's entry, the same function.
            let last = |it: &str| it.rsplit("::").next().unwrap_or(it).to_owned();
            if !functions.iter().any(|it| it == name || last(it) == name) {
                functions.retain(|it| *it != last(name) || name == it);
                functions.push(name.to_owned()); // alloc: as above
            }
        }
    }
    Why {
        wasted: report.severity == Severity::Warning,
        cause,
        headline: report.headline.clone(), // clone: as above
        functions,
        more: report.rule.as_ref().map(|rule| format!("blaze explain {rule}")), // alloc: as above
    }
}

/// cargo's `machine_applicable`: a child of the diagnostic suggests a fix `check --fix` makes.
fn fixable(diagnostic: DiagnosticRef<'_>) -> bool {
    diagnostic.children.iter().any(|child| {
        child
            .spans
            .iter()
            .any(|span| span.applicability == Some(blaze_api::Applicability::MachineApplicable))
    })
}

#[cfg(test)]
mod tests;
