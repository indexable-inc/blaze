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
//! does, never a copy of it. Once the printer's tables have grown to the build's size, printing
//! a response allocates nothing.
//!
//! `--message-format=short` prints as `human` does (cargo's `on_stderr_line_inner` takes both
//! one way): the daemon renders each diagnostic in rustc's short form, `file:line:col: level:
//! message`, from the `--message-format` in `Cargo::args`, and cargo's own lines are the same.

use std::{
    fmt::Write as _,
    hash::{BuildHasher as _, BuildHasherDefault},
    io::{self, Write},
    path::Path,
    time::Duration,
};

use blaze_api::{Applicability, Artifact, DiagnosticRef, FetchVerb, Level, ResponseRef, TargetKind, UnitOfRef, Verb};
use blaze_hash::FixedState;

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
                Format::Json { short, ansi, render_diagnostics } => set(short, ansi, render_diagnostics),
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
}

const _: () = assert!(size_of::<Counted>() == 28);

/// What a unit's summary names it by, kept from the first response of it: its package's name
/// and its target's, each a range of [`Printer::names`], its target's kind and its mode.
struct Unit {
    package: Name,
    target: Name,
    kind: TargetKind,
    test: bool,
    /// A path package, whose summary hints at `cargo fix`.
    local: bool,
}

const _: () = assert!(size_of::<Unit>() == 20);

/// A range of [`Printer::names`]: where it starts, and its length.
#[derive(Clone, Copy)]
struct Name(u32, u32);

pub(super) type ByHash<V> = blaze_hash::ByHash<u64, V>;
type HashedSet = blaze_hash::ByHashSet<u64>;

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
    printed: HashedSet,
    built: Option<Built>,
    /// The daemon numbered a unit or package past [`IDS`]: said, and the build fails.
    refused: bool,
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
            printed: HashedSet::with_capacity_and_hasher(DIAGNOSTICS, BuildHasherDefault::default()),
            built: None,
            refused: false,
        }
    }

    /// Whether the daemon numbered a unit or package past any request's, which was said as an
    /// error: the build is then a failure whatever its Done said.
    pub fn refused(&self) -> bool {
        self.refused
    }

    /// The daemon's `index` of a `what` is past [`IDS`]: said as an error, and the build fails.
    fn refuse(&mut self, what: &str, index: u32) -> io::Result<()> {
        self.to(false)?;
        self.refused = true;
        say::leveled_with(&mut self.line, say::ERROR, "error", self.shown.color, |line| {
            // Writing to a String cannot fail.
            let _ = write!(line, "the daemon numbered a {what} {index}, past the {IDS} a request's indexes stay under");
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
                summary(&mut self.line, &self.units[at], &self.names, &self.counted[at], self.shown);
            }
            // blaze's own, for the proof harness: what the request did, then what became of each
            // dependency crate's check.
            // How each image was linked goes to stderr in every format: a link that was not
            // spliced is never silent.
            ResponseRef::Work(worked) => {
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
            ResponseRef::Error(error) => {
                self.to(false)?;
                say::error_into(&mut self.line, error.what, self.shown.color);
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
            || (diagnostic.spans.is_empty() && diagnostic.code.is_none() && summarizes(diagnostic.message))
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
        // cargo takes one newline off the end, then writes the text and a newline.
        let rendered = diagnostic.rendered.strip_suffix('\n').unwrap_or(diagnostic.rendered);
        let printed = match renders {
            true => self.printed.insert(FixedState.hash_one(rendered)),
            false => true,
        };
        if renders && printed {
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
        self.units.push(Unit { package, target, kind: unit.target.kind, test: unit.test, local });
        let warned = say::Warned { fixable: Some(0), ..say::Warned::default() };
        self.counted.push(Counted { warned, ..Counted::default() });
        Ok(Some(row))
    }

    /// The build is over (`ok` is its `Done`'s), `took` after it began: each unit's summary,
    /// `Finished` and `build-finished`, all written.
    pub fn finish(&mut self, ok: bool, took: Duration) -> io::Result<()> {
        let color = self.shown.color;
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
        if self.shown.format.json() {
            self.to(true)?;
            json::build_finished(&mut self.line, ok);
            self.line.push('\n');
        }
        self.flush()?;
        self.out.flush()?;
        self.err.flush()
    }
}

/// rustc's closing lines, which cargo replaces with its own summary.
fn summarizes(message: &str) -> bool {
    message.starts_with("aborting due to") || message.ends_with(" warning emitted") || message.ends_with(" warnings emitted")
}

/// A unit's "generated N warnings" (`JobQueue::report_warning_count` as the unit finishes, a
/// failed one too, silent under `-q`), then its "could not compile" (the unit's error, which
/// `-q` does not silence), into `line`.
fn summary(line: &mut String, unit: &Unit, names: &str, counted: &Counted, shown: Shown) {
    let package = named(names, unit.package);
    let target = say::Named { kind: unit.kind, name: named(names, unit.target) };
    if !shown.quiet && counted.warned.total > 0 {
        say::leveled_with(line, say::WARN, "warning", shown.color, |line| {
            say::generated_into(line, package, target, unit.test, unit.local, counted.warned);
        });
    }
    if counted.errors > 0 {
        say::leveled_with(line, say::ERROR, "error", shown.color, |line| {
            say::could_not_compile_into(line, package, target, unit.test, counted.errors, counted.warnings);
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
    diagnostic
        .children
        .iter()
        .any(|child| child.spans.iter().any(|span| span.applicability == Some(Applicability::MachineApplicable)))
}

#[cfg(test)]
mod tests;
