//! The `Lints` stage of `blaze engine-check` (docs/v2/plan/design/lints-stage.md, D135): ONE
//! task a FILE, every selected token rule in one walk of each unit it relints, its record the
//! file's units' keys with their findings SPARSE (rule 77): a clean unit holds no row and no
//! record of its own.
//!
//! THE PLAN, from text: one `Lint` task an outline that lists lint units (lints.rs `parts_in`:
//! unit 0 the file with its top-level items as holes, one unit an outline entry with its children
//! as holes, each a `Lint` part keyed by what its pass reads, lints.rs `Structural`; then the
//! file's own `Lint` part over them, lints.rs `file_part`), in `Phase { stage: Lints, wave: 0 }`,
//! named by its file ([`crate::lints::file_lints_name`]), its source the file part's (every
//! unit's name and key) with the selected rule set's id and the edition folded in ([`plan`]). The
//! files outlined are the members' (found forward from their crate roots), so only workspace
//! members are linted. A replay of a file whose units' keys all stand enters nothing, and the
//! answer holds one lint row a file, however many items it has.
//!
//! THE TASK, on the pool: its file's text read once and checked against the id its outline's key
//! was made from, NOT outlined again (its entries and chains read in place from the planner's
//! outline record, [`lint_text`]); its units listed from the outline's `Lint` parts. A unit whose
//! name and key the file's last lint record lists (under the same tool, rule set and edition)
//! is valid by its key: its findings are copied from that record, it is not linted (a function
//! of its key alone, so the copy is the bytes a relint would make: rule 26, tested). Every other
//! unit is linted by blaze-lint in one pass over its own text (lints.rs `lint_units_in`: its
//! ancestors walked for its scope, a file of many relinted units split into batches on the pool;
//! blaze-lint lib.rs: one lex, each token dispatched to the rules registered for its kind). The
//! last record is a memo, not a read: nothing in the result depends on whether it was there.
//!
//! THE RECORD ([`file_record`]): its payload the form [`FILE_FORM`], what it was made under
//! ([`Under`]: the `Lint` tool, the rule set's id, the edition), every unit's name and key in
//! name order, then each unit WITH findings (its place in that list, its item's chain, each
//! finding's rule number, range of the unit's own text and words, then the engine's findings
//! with their fixes, lints.rs `payload`): whole in the payload, as the store keeps a `Lint` row's
//! record (no drive findings); this request's drive findings are those units' findings in that
//! order, for its lines. A unit in the key list and not in the findings list is clean. Its row: `WHOLE` defined
//! at the record's out (a digest of what prints and the fixes, never of a key or a cost), no
//! read, its findings' count.
//!
//! THE LINES: each finding an own-placed entry (engine_lines.rs `PLACED_OWN`): its unit's item by
//! its chain (none for the file's unit), its children's lengths given back from today's outline
//! when printed; no level is stored (rule 81): the printer gives each the level `shown.lints`
//! says. What the jobs read is counted in the profile (lints.rs `LintCounts`).

use core::convert::Infallible;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_drive::{Kind, Locate, Name, Part, TaskFailure, Why};
use blaze_engine::{Buffer, Done, Phase, Planned, SVec, Stage, Task};
use blaze_ids::{ContentId, Hasher};
use blaze_lint::{Cost, Rule, RuleSet};
use blaze_own::SharedAlloc;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use bstd::path::Path;
use bstd::task::Class;
use std::alloc::Global;

use crate::CliError;
use crate::engine_check::EngineCheckError;
use crate::engine_lines::Lines;
use crate::engine_tasks::{Outlined, WHOLE, edition_source};
use crate::lints::{AskedUnit, LintCounts};
use crate::work::{self, Record};

/// A file's lint job's frame: its text, its outline's entries, its units' scopes and findings.
const LINTS_FRAME: usize = 1 << 28;

/// 3: one task a FILE (lints.rs `file_part`), its source the file part's; 2: one task a unit, its
/// record ranges of its own text; 1: ranges of its whole text, holes and all.
const SOURCE_DOMAIN: &str = "blaze-cli engine lint task source 3";

/// The form of a file's lint record's payload ([`file_record`]). A record of another form is
/// refused by name when it is read back ([`Prior::of`]), never taken as no record (rule 3).
const FILE_FORM: &[u8; 4] = b"BLF1";

/// The out of a file's lint record ([`file_record`]).
const FILE_OUT_DOMAIN: &str = "blaze-cli engine lint file out 1";

/// What refuses a lint task here (rule 19).
const RULE: &str = "blaze-cli engine_lints.rs";

/// One `Lint` task for each outline (sorted by name) that lists its file's `Lint` part, whatever
/// the step's rule set: the set a unit runs is the step's joined with what its file's marks add
/// (lints.rs `lint_units_in`, blaze-lint `RootMarks::rules`), so an empty step set is no reason
/// to skip a file that denies a lint or is a sans-IO core. The marks are not in the outline's
/// record, so the plan cannot tell those files apart here; an unmarked file under an empty set
/// makes a record of no findings, kept by its key after the first run.
/// Its source (rule 21): the file part's (every unit's name and key, lints.rs `file_part`; a
/// unit's key holds its marks' key), the rule set's id (a change of the step or of `lints-add`
/// relints every unit and nothing else) and the edition.
pub(crate) fn plan<'h>(outlined: &[Outlined<'_>], external: &[Name], rules: &RuleSet, alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned = SVec::new_in(alloc);
    let set = rules.id();
    for one in outlined.iter() {
        if external.binary_search(&one.name).is_ok() {
            continue;
        }
        let Ok(record) = &one.made else { continue };
        let name = crate::lints::file_lints_name(crate::signature::SignatureFile::new(&one.path).subject());
        let Ok(at) = record.parts.binary_search_by_key(&name, |part| part.name) else { continue };
        let Locate::Source(whole) = record.parts[at].locate else { continue };
        let mut h = Hasher::new(SOURCE_DOMAIN);
        h.update(&whole.0).update(&set);
        planned.push(Planned { name, phase: Phase::new(Stage::Lints, 0), source: edition_source(h.finish(), one.edition), file: one.name });
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// What a file's lint record is made under, beside its units' keys: a unit's findings are a
/// function of its key and these (the code that lints, the rule set, the edition the text is
/// lexed in), so a unit is kept from a last record only when they are equal ([`Prior`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Under {
    /// The `Lint` kind's tool (home.rs `tools`: the producer's identity).
    pub(crate) tool: ContentId,
    /// The selected rule set's id (blaze-lint `RuleSet::id`).
    pub(crate) rules: [u8; 32],
    pub(crate) edition: u8,
}

/// One unit of a file as its outline lists it: its `Lint` part's name, key and place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct UnitPart {
    pub(crate) name: Name,
    pub(crate) key: ContentId,
    pub(crate) place: u32,
}

/// The units an outline's `parts` list, but the file's own `Lint` part `whole`, in name order
/// (the parts are sorted by name, work.rs `outline_file`).
pub(crate) fn units_of(parts: &[Part], whole: Name) -> std::vec::Vec<UnitPart, Global> {
    let mut units = std::vec::Vec::new_in(Global);
    for part in parts.iter() {
        if part.name.kind != Kind::Lint || part.name == whole {
            continue;
        }
        if let Locate::Source(key) = part.locate {
            units.push(UnitPart { name: part.name, key, place: part.place });
        }
    }
    units
}

/// One unit with findings, as a file's lint record holds it: its name, its item's chain (`None`
/// for the file's unit), its drive findings (rule, own start, own end, and its words' run in
/// `words`) and the engine's findings with their fixes (lints.rs `payload`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Found {
    pub(crate) name: Name,
    pub(crate) chain: Option<u64>,
    pub(crate) findings: std::vec::Vec<(u16, u32, u32, usize, usize), Global>,
    pub(crate) words: std::vec::Vec<u8, Global>,
    pub(crate) fixes: std::vec::Vec<u8, Global>,
}

/// A file's last lint record, read back ([`Prior::of`]): what it was made under, every unit's
/// name and key in name order, and the units with findings.
pub(crate) struct Prior {
    under: Under,
    units: std::vec::Vec<(Name, ContentId), Global>,
    found: std::vec::Vec<Found, Global>,
}

/// Why a last lint record could not be read back (rule 19): its task is refused with it.
const UNFORMED: &str = "its file's last lint record is not of this form (engine_lints.rs `FILE_FORM`)";
const UNREAD: &str = "its file's last lint record ends inside a field or lists a unit it does not hold (engine_lints.rs `Prior::of`)";

/// Little-endian fields read off a payload, each length checked.
struct Fields<'b> {
    bytes: &'b [u8],
}

impl<'b> Fields<'b> {
    fn take(&mut self, n: usize) -> Result<&'b [u8], &'static str> {
        if self.bytes.len() < n {
            return Err(UNREAD);
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn id(&mut self) -> Result<[u8; 32], &'static str> {
        self.take(32)?.try_into().map_err(|_| UNREAD)
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| UNREAD)?))
    }

    fn u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| UNREAD)?))
    }
}

impl Prior {
    /// `record` (a file's lint record, [`file_record`]'s) read back: every field checked, so a
    /// record of another form or one cut short is a named refusal, never a record of no unit.
    pub(crate) fn of(record: &Record<Global>) -> Result<Prior, &'static str> {
        let payload = record.payload_bytes().map_err(|_| UNREAD)?;
        let mut at = Fields { bytes: payload };
        if at.take(4).map_err(|_| UNFORMED)? != FILE_FORM {
            return Err(UNFORMED);
        }
        let under = Under { tool: ContentId(at.id()?), rules: at.id()?, edition: at.u8()? };
        let count = at.u32()? as usize;
        let mut units = std::vec::Vec::with_capacity_in(count.min(payload.len() / 64), Global);
        for _ in 0..count {
            units.push((Name::new(Kind::Lint, ContentId(at.id()?)), ContentId(at.id()?)));
        }
        let with = at.u32()? as usize;
        let mut found = std::vec::Vec::with_capacity_in(with.min(payload.len()), Global);
        for _ in 0..with {
            let unit = at.u32()? as usize;
            let name = units.get(unit).ok_or(UNREAD)?.0;
            let chain = match at.u8()? {
                0 => {
                    at.u64()?;
                    None
                }
                1 => Some(at.u64()?),
                _ => return Err(UNREAD),
            };
            let findings = at.u32()? as usize;
            let mut one = Found { name, chain, findings: std::vec::Vec::with_capacity_in(findings.min(at.bytes.len() / 14), Global), words: std::vec::Vec::new_in(Global), fixes: std::vec::Vec::new_in(Global) };
            for _ in 0..findings {
                let rule = u16::from_le_bytes(at.take(2)?.try_into().map_err(|_| UNREAD)?);
                let (start, end) = (at.u32()?, at.u32()?);
                let words = at.u32()? as usize;
                let words = at.take(words)?;
                let first = one.words.len();
                one.words.extend_from_slice(words);
                one.findings.push((rule, start, end, first, one.words.len()));
            }
            let fixes = at.u32()? as usize;
            one.fixes.extend_from_slice(at.take(fixes)?);
            found.push(one);
        }
        if !at.bytes.is_empty() {
            return Err(UNREAD);
        }
        Ok(Prior { under, units, found })
    }

    /// Whether unit `name` of key `key` is kept by its key under `under`, and its findings when
    /// it has any: `None` when it is to be linted.
    fn kept(&self, under: &Under, name: Name, key: ContentId) -> Option<Option<&Found>> {
        if self.under != *under {
            return None;
        }
        let at = self.units.binary_search_by_key(&name, |unit| unit.0).ok()?;
        if self.units[at].1 != key {
            return None;
        }
        Some(self.found.binary_search_by_key(&name, |one| one.name).ok().map(|at| &self.found[at]))
    }
}

/// A file's lint record, named `name`, made under `under` over its `units` (name order) and the
/// units with findings `found` (name order, each one of `units`): the form [`FILE_FORM`] (see
/// the module docs), and each found unit's chain with the run of its findings in the record.
/// A function of its inputs alone: relinted or kept, a unit's [`Found`] is the same bytes, so
/// the record is (rule 26).
pub(crate) fn file_record(name: Name, under: &Under, units: &[UnitPart], found: &[Found]) -> Result<(Record<Global>, std::vec::Vec<(Option<u64>, usize, usize), Global>), &'static str> {
    let mut made = Record::new_in(name, Global);
    let mut placed = std::vec::Vec::with_capacity_in(found.len(), Global);
    let word = |into: &mut blaze_own::Vec<u8, Global>, value: u32| into.extend_from_slice(&value.to_le_bytes());
    // Written in place, the record's own payload (rule 11); its findings and messages beside.
    let mut payload = core::mem::replace(&mut made.payload, blaze_own::Vec::new_in(Global));
    payload.extend_from_slice(FILE_FORM);
    payload.extend_from_slice(&under.tool.0);
    payload.extend_from_slice(&under.rules);
    payload.push(under.edition);
    word(&mut payload, u32::try_from(units.len()).map_err(|_| "a file of more lint units than a record counts")?);
    for unit in units.iter() {
        payload.extend_from_slice(&unit.name.subject.0);
        payload.extend_from_slice(&unit.key.0);
    }
    word(&mut payload, found.len() as u32);
    let mut out = Hasher::new(FILE_OUT_DOMAIN);
    for one in found.iter() {
        let unit = units.binary_search_by_key(&one.name, |unit| unit.name).map_err(|_| "a lint unit with findings its file's outline does not list")?;
        word(&mut payload, unit as u32);
        payload.push(u8::from(one.chain.is_some()));
        payload.extend_from_slice(&one.chain.unwrap_or(0).to_le_bytes());
        word(&mut payload, one.findings.len() as u32);
        // What prints and what `--fix` reads: never the unit's key or place.
        out.update(&one.name.subject.0).update(&[u8::from(one.chain.is_some())]).update(&one.chain.unwrap_or(0).to_le_bytes());
        out.update(&(one.findings.len() as u32).to_le_bytes());
        let from = made.findings.len();
        for &(rule, start, end, w_from, w_to) in one.findings.iter() {
            let words = one.words.get(w_from..w_to).ok_or("a lint finding's words past its unit's")?;
            // In the payload too: the record is read back whole from the store, whose drive
            // record keeps no findings of a `Lint` row ([`Prior::of`]).
            payload.extend_from_slice(&rule.to_le_bytes());
            payload.extend_from_slice(&start.to_le_bytes());
            payload.extend_from_slice(&end.to_le_bytes());
            word(&mut payload, words.len() as u32);
            payload.extend_from_slice(words);
            out.update(&rule.to_le_bytes()).update(&start.to_le_bytes()).update(&end.to_le_bytes()).update(&(words.len() as u32).to_le_bytes()).update(words);
            let at = made.messages.len();
            made.messages.extend_from_slice(words);
            made.findings.push((rule, start, end, at, made.messages.len()));
        }
        word(&mut payload, one.fixes.len() as u32);
        payload.extend_from_slice(&one.fixes);
        out.update(&(one.fixes.len() as u32).to_le_bytes()).update(&one.fixes);
        placed.push((one.chain, from, made.findings.len()));
    }
    made.payload = payload;
    made.out = out.finish();
    Ok((made, placed))
}

/// One file's task as its job made it, all in `Global` (it crosses to the coordinator).
pub(crate) struct Made {
    pub(crate) name: Name,
    /// Its file, by its place in the planner's outlines.
    file: usize,
    /// Each found unit's item chain and the run of its findings in the record ([`file_record`]).
    placed: std::vec::Vec<(Option<u64>, usize, usize), Global>,
    /// Each unit this request linted, its item's name as written (`None` for the file's unit).
    relinted: std::vec::Vec<Option<blaze_own::String<Global>>, Global>,
    /// The units the record lists.
    units: usize,
    /// What the relinted units' passes cost: their own tokens and each rule's sites.
    cost: (u64, Cost),
    pub(crate) record: Record<Global>,
}

/// One file's job as the worker made it ([`lint_text`]): its record and what the request keeps
/// of it.
pub(crate) struct FileLinted {
    pub(crate) record: Record<Global>,
    placed: std::vec::Vec<(Option<u64>, usize, usize), Global>,
    pub(crate) relinted: std::vec::Vec<Option<blaze_own::String<Global>>, Global>,
    units: usize,
    cost: (u64, Cost),
}

/// One file's job, made on the coordinator: its outline's place, its task, what the worker reads
/// of the outline (its path, source and edition, its record's payload in place: the planner's
/// outline, never made again) and its units, and the file's last lint record when there is one.
struct FileJob<'p> {
    file: usize,
    task: Task,
    text: FileText<'p>,
    items: &'p [u8],
    units: std::vec::Vec<UnitPart, Global>,
    prior: Option<&'p Prior>,
}

/// What one request's `Lints` stage saw, for the command's tests: each unit it linted as
/// `path::item` (`path` alone for a file's unit), sorted, the findings its files' records hold,
/// and what its file jobs read and lexed (lints.rs `LintCounts`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LintsSeen {
    pub(crate) entered: std::vec::Vec<std::string::String, Global>,
    pub(crate) findings: u32,
    pub(crate) counts: LintCounts,
}

/// [`LintCounts`] summed from the stage's file jobs on any worker (the `Ready` driver's run
/// reads the stage, never writes it: [`LintsStage::lint_units`]). Each a sum, so its value is
/// the same in any order the jobs end; read after the jobs are joined.
struct Tally([AtomicU64; 8]);

impl Tally {
    const fn new() -> Tally {
        Tally([const { AtomicU64::new(0) }; 8])
    }

    fn add(&self, counts: &LintCounts) {
        for (sum, value) in self.0.iter().zip(counts.row()) {
            sum.fetch_add(value, Ordering::Relaxed);
        }
    }

    fn get(&self) -> LintCounts {
        LintCounts::of_row(core::array::from_fn(|at| self.0[at].load(Ordering::Relaxed)))
    }
}

/// The stage: the selected rules and the tool they run under, each entered file's last lint
/// record, each task's result and refusal.
pub(crate) struct LintsStage {
    rules: RuleSet,
    /// The `Lint` kind's tool ([`Under::tool`]).
    tool: ContentId,
    /// Each entered task's last record, read back, by name ([`LintsStage::remember`]).
    priors: std::vec::Vec<(Name, Result<Prior, &'static str>), Global>,
    /// Each task entered and made, sorted by name after its phase.
    made: std::vec::Vec<Made, Global>,
    /// Each task refused, and why (rule 19).
    refused: std::vec::Vec<(Name, Why), Global>,
    /// What the file jobs read and lexed.
    counts: Tally,
}

impl LintsStage {
    pub(crate) fn new(rules: RuleSet, tool: ContentId) -> LintsStage {
        LintsStage { rules, tool, priors: std::vec::Vec::new_in(Global), made: std::vec::Vec::new_in(Global), refused: std::vec::Vec::new_in(Global), counts: Tally::new() }
    }

    /// Task `name`'s result, when it was entered and made this request.
    pub(crate) fn result(&self, name: Name) -> Option<&Made> {
        self.made.binary_search_by_key(&name, |one| one.name).ok().map(|at| &self.made[at])
    }

    /// Task `name`'s last record (the stored answer's, engine_check.rs `lint_priors`), kept as
    /// what its units may be kept by key from. Read back here, once; one that does not read
    /// refuses the task by name when it runs (rule 3).
    pub(crate) fn remember(&mut self, name: Name, record: &Record<Global>) {
        let prior = Prior::of(record);
        match self.priors.binary_search_by_key(&name, |one| one.0) {
            Ok(at) => self.priors[at].1 = prior,
            Err(at) => self.priors.insert(at, (name, prior)),
        }
    }

    /// Task `task`'s job over the planner's outlines: its file's outline, its units, its last
    /// record; or why it is refused.
    fn job_of<'p>(&'p self, outlined: &'p [Outlined<'_>], task: &Task) -> Result<FileJob<'p>, Why> {
        let file = outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)).map_err(|_| Why::Refused { what: "a `Lint` task whose file is none of the planner's outlines", rule: RULE })?;
        let one = &outlined[file];
        let record = one.made.as_ref().map_err(|_| Why::Refused { what: "a `Lint` task whose file's outline failed", rule: RULE })?;
        let name = task.planned.name;
        if part_place(&record.parts, name).is_none() {
            return Err(Why::Refused { what: "a `Lint` task its file's outline lists no `Lint` part of", rule: RULE });
        }
        let items = record.payload_bytes().map_err(|_| Why::Refused { what: "a `Lint` task whose file's outline record holds no payload", rule: RULE })?;
        let prior = match self.priors.binary_search_by_key(&name, |one| one.0) {
            Ok(at) => Some(self.priors[at].1.as_ref().map_err(|what| Why::Refused { what: *what, rule: RULE })?),
            Err(_) => None,
        };
        let text = FileText { path: &one.path[..], source: one.source, edition: one.edition };
        Ok(FileJob { file, task: *task, text, items, units: units_of(&record.parts, name), prior })
    }

    /// What a job's record is made under.
    fn under(&self, job: &FileJob<'_>) -> Under {
        Under { tool: self.tool, rules: self.rules.id(), edition: job.text.edition }
    }

    /// Job `job` run here, on a worker: its counts added to the stage's.
    fn run_job(&self, cx: &Cx, root: &Dir<ReadWrite>, job: &FileJob<'_>) -> Result<FileLinted, Why> {
        let mut counts = LintCounts::default();
        let under = self.under(job);
        let made = work::unwinding(|| lint_file(cx, root, job, &under, &self.rules, &mut counts));
        self.counts.add(&counts);
        match made {
            Ok(made) => made,
            Err(work::Panicked) => Err(Why::Refused { what: "its file's lint job panicked; that file's lint task alone failed", rule: RULE }),
        }
    }

    /// The Lints phase: each entered file linted at once on the workers, one job a file, each
    /// task's row in `buffer`.
    #[expect(clippy::too_many_arguments, reason = "a phase's edge: each argument one handle of it")]
    pub(crate) fn run_phase<'a>(
        &mut self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        outlined: &[Outlined<'_>],
        workers: NonZeroU32,
        clock: &Clock,
        tasks: &[Task],
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: the lints phase");
        let mut made: std::vec::Vec<Made, Global> = std::vec::Vec::new_in(Global);
        let mut refused: std::vec::Vec<(Name, Why), Global> = std::vec::Vec::new_in(Global);
        {
            let stage = &*self;
            // 1. Each task to its file's job.
            let mut jobs: std::vec::Vec<FileJob<'_>, Global> = std::vec::Vec::with_capacity_in(tasks.len(), Global);
            for task in tasks.iter() {
                match stage.job_of(outlined, task) {
                    Ok(job) => jobs.push(job),
                    Err(why) => {
                        buffer.done.push(crate::engine_tasks::failed(task, alloc));
                        refused.push((task.planned.name, why));
                    }
                }
            }
            // 2. Each file's job, at once, on the workers.
            type Ran = Result<FileLinted, Why>;
            let mut worked: std::vec::Vec<(Ran, u64), Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
            {
                let scope = cx.scope();
                scope.narrow_workers(workers, Class::Foreground);
                let mut running = std::vec::Vec::with_capacity_in(jobs.len(), Global);
                for job in jobs.iter() {
                    let clock = clock.clone();
                    running.push(scope.may_overlap("engine lints", LINTS_FRAME, move |cx: &Cx| {
                        let began = clock.now();
                        let ran = stage.run_job(cx, root, job);
                        Ok::<_, Infallible>((ran, clock.now().since(began).map_or(0, crate::engine_times::ns)))
                    }));
                }
                let mut stopped = None;
                for handle in running.into_iter() {
                    match handle.join(&cx) {
                        Ok(one) => worked.push(one),
                        Err(error) => {
                            stopped.get_or_insert(error);
                        }
                    }
                }
                scope.join(&cx).map_err(CliError::Pool)?;
                if let Some(error) = stopped {
                    return Err(CliError::Pool(error));
                }
            }
            // 3. Each result as the engine's row, in task order.
            for (job, (ran, ns)) in jobs.iter().zip(worked) {
                times.push(job.task.planned.name, ns);
                match ran.and_then(|linted| file_done(&job.task, job.file, linted, alloc)) {
                    Ok((done, one)) => {
                        buffer.done.push(done);
                        made.push(one);
                    }
                    Err(why) => {
                        buffer.done.push(crate::engine_tasks::failed(&job.task, alloc));
                        refused.push((job.task.planned.name, why));
                    }
                }
            }
        }
        self.made.extend(made);
        self.refused.extend(refused);
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }

    /// Task `name`'s block of the request's lines (engine_lines.rs): each found unit's findings
    /// by their start, end and rule (rule 25), each own range placed in its unit by the item's
    /// chain (none for the file's unit); a refused task's note. Nothing when it was not entered.
    /// A finding whose edition is not known is a typed stop naming the task, never dropped
    /// (rule 3).
    pub(crate) fn lines(&self, name: Name, outlined: &[Outlined<'_>], lines: &mut Lines<'_>, frame: &Frame) -> Result<(), EngineCheckError> {
        if let Some(one) = self.result(name) {
            let file = outlined.get(one.file).ok_or(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" })?;
            let edition = blaze_surface::Edition::from_byte(file.edition).ok_or(EngineCheckError::Unplaced { body: name, what: "its file's edition byte is no edition" })?;
            let record = &one.record;
            let mut order: std::vec::Vec<usize, Global> = std::vec::Vec::new_in(Global);
            for &(chain, from, to) in one.placed.iter() {
                if from > to || to > record.findings.len() {
                    return Err(EngineCheckError::Unplaced { body: name, what: "a lint unit's findings past its record's" });
                }
                order.clear();
                order.extend(from..to);
                order.sort_unstable_by_key(|at| {
                    let (rule, start, end, _, _) = record.findings[*at];
                    (start, end, rule)
                });
                for &at in order.iter() {
                    let (rule, start, end, w_from, w_to) = record.findings[at];
                    let words = &record.messages[w_from..w_to];
                    // A range of the unit's own text: the printer gives its children their
                    // lengths from today's outline (engine_lines.rs `PLACED_OWN`, rule 71).
                    lines.placed_own(name, rule, start, end, &file.path, chain, edition, words);
                }
            }
            return Ok(());
        }
        if let Some((_, why)) = self.refused.iter().find(|(refused, _)| *refused == name) {
            let failure = TaskFailure { name, why: *why };
            lines.note(name, crate::engine_lines::Class::Failure, bstd::mem::format_in!(frame, "a lint task failed: {failure}").as_bytes());
        }
        Ok(())
    }

    /// The profile's lines: the tasks entered and their findings, then each rule that tried a
    /// site or found something, by its id, with its cost (the sites it tried in the units linted
    /// this request, deterministic) and its findings; then the tokens those units' lexes made,
    /// the cost every rule shares; then what the file jobs read and the rows they wrote.
    pub(crate) fn profile(&self, into: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        let mut cost = Cost::NONE;
        let mut tokens = 0u64;
        let mut found = [0u32; Rule::COUNT];
        let (mut units, mut relinted) = (0usize, 0usize);
        for one in self.made.iter() {
            tokens = tokens.saturating_add(one.cost.0);
            cost.add_all(&one.cost.1);
            units += one.units;
            relinted += one.relinted.len();
            for (rule, ..) in one.record.findings.iter() {
                if let Some(rule) = crate::lints::rule_of(*rule) {
                    found[rule as usize - 1] = found[rule as usize - 1].saturating_add(1);
                }
            }
        }
        let entered = self.made.len() + self.refused.len();
        let findings: u32 = found.iter().sum();
        // A report: a full frame cuts it short where it stood (engine_check.rs `profile_text`).
        let _ = write!(into, "\nlints: {entered} tasks, {findings} findings; by rule:");
        let mut first = true;
        for rule in Rule::ALL {
            let (sites, hits) = (cost.of(rule), found[rule as usize - 1]);
            if sites == 0 && hits == 0 {
                continue;
            }
            let _ = write!(into, "{} {} {sites} {hits}", if first { "" } else { "," }, rule.name());
            first = false;
        }
        let _ = writeln!(into);
        let _ = writeln!(into, "lints: {tokens} tokens lexed, once for every rule; {} task(s) refused", self.refused.len());
        // What the file jobs read (lints.rs `LintCounts`, its floor stated there) and the rows
        // they wrote: one a file, its clean units in its key list alone.
        let c = self.counts.get();
        let clean = self.made.iter().filter(|one| one.record.findings.is_empty()).count();
        let _ = writeln!(
            into,
            "lints: files read {}, outlined {}; units walked {} (scopes, no rule), linted {}; own bytes lexed {}; whole-file facts: variant-count {}, own heads {}; batches {}",
            c.files_read, c.files_outlined, c.units_walked, c.units_linted, c.bytes_lexed, c.variants_gathered, c.heads_read, c.batches
        );
        let _ = writeln!(
            into,
            "lints: rows written {} (one a file), {clean} clean, {} with findings; units listed {units}, relinted {relinted}, kept by key {}",
            self.made.len(),
            self.made.len() - clean,
            units.saturating_sub(relinted)
        );
    }

    /// What the command's tests read ([`LintsSeen`]).
    pub(crate) fn seen(&self, outlined: &[Outlined<'_>]) -> LintsSeen {
        let mut entered = std::vec::Vec::new_in(Global);
        let mut findings = 0u32;
        for one in self.made.iter() {
            let path = outlined.get(one.file).and_then(|file| core::str::from_utf8(&file.path).ok()).unwrap_or("?");
            for item in one.relinted.iter() {
                entered.push(match item {
                    Some(item) => std::string::String::from(path) + "::" + item.as_str(),
                    None => std::string::String::from(path),
                });
            }
            findings = findings.saturating_add(u32::try_from(one.record.findings.len()).unwrap_or(u32::MAX));
        }
        entered.sort_unstable();
        LintsSeen { entered, findings, counts: self.counts.get() }
    }
}

/// The file a lint job reads, as its outline names it: its path, the id of its text with its
/// edition folded in (engine_tasks.rs `edition_source`) and its edition byte.
#[derive(Clone, Copy)]
struct FileText<'p> {
    path: &'p [u8],
    source: ContentId,
    edition: u8,
}

/// One file's row (`WHOLE` defined at the record's out, no read, its findings' count) and what
/// the request keeps of it: the one conversion from a worker's [`FileLinted`] to the engine's
/// `Done`, for the phase path's join and the `Ready` driver's [`LintsStage::lint_units`] alike
/// (rule 30).
fn file_done<'a>(task: &Task, file: usize, linted: FileLinted, alloc: SharedAlloc<'a>) -> Result<(Done<'a>, Made), Why> {
    let FileLinted { record, placed, relinted, units, cost } = linted;
    let name = task.planned.name;
    let mut defines = SVec::with_capacity_in(1, alloc);
    defines.push((WHOLE, record.out));
    let findings = u32::try_from(record.findings.len()).unwrap_or(u32::MAX);
    let done = Done { name, key: task.key, out: record.out, defines, reads: SVec::new_in(alloc), findings, failed: false };
    Ok((done, Made { name, file, placed, relinted, units, cost, record }))
}

/// What the request keeps of one task the `Ready` driver ran ([`LintsStage::lint_units`]): its
/// result, or its refusal and why, merged into the stage after the drive by [`LintsStage::keep`]
/// (a run reads the stage, never writes it: it runs on any worker).
pub(crate) enum UnitKept {
    Made(Made),
    Refused(Name, Why),
}

impl LintsStage {
    /// A file's entered `Lint` task for the `Ready` driver (D187), on this worker: the file's
    /// job as the phase path's ([`LintsStage::run_job`]), each task's row with what the request
    /// keeps of it, in `asked`'s order. `file` is the outline's place in the planner's outlines
    /// (the job finds it again by the task's file); `asked` each task with its part's source as
    /// the outline lists it (its key's, not read again: the file's text is checked against the
    /// outline's id).
    pub(crate) fn lint_units<'a>(&self, cx: &Cx, root: &Dir<ReadWrite>, file: usize, one: &Outlined<'_>, asked: &[(Task, ContentId)], alloc: SharedAlloc<'a>) -> SVec<'a, (Done<'a>, UnitKept)> {
        let mut rows: SVec<'a, (Done<'a>, UnitKept)> = SVec::with_capacity_in(asked.len(), alloc);
        let refused = |task: &Task, why: Why| (crate::engine_tasks::failed(task, alloc), UnitKept::Refused(task.planned.name, why));
        let outlined = core::slice::from_ref(one);
        for (task, _) in asked.iter() {
            let ran = self.job_of(outlined, task).and_then(|mut job| {
                job.file = file;
                self.run_job(cx, root, &job).and_then(|linted| file_done(task, file, linted, alloc))
            });
            rows.push(match ran {
                Ok((done, kept)) => (done, UnitKept::Made(kept)),
                Err(why) => refused(task, why),
            });
        }
        rows
    }

    /// What a `Ready` run of a file made, kept as the phase path keeps it: a result in name order
    /// (as [`LintsStage::result`] searches), or a refusal.
    pub(crate) fn keep(&mut self, kept: UnitKept) {
        match kept {
            UnitKept::Made(made) => {
                let at = self.made.partition_point(|one| one.name < made.name);
                self.made.insert(at, made);
            }
            UnitKept::Refused(name, why) => self.refused.push((name, why)),
        }
    }
}

/// One file's lint job on a worker, in its task's frame: its text read once and checked against
/// the id its outline was made from (a difference is [`Why::Changed`]: no re-read, rule 3), then
/// [`lint_text`] over the planner's outline record. What it did is added to `counts`.
fn lint_file(cx: &Cx, root: &Dir<ReadWrite>, job: &FileJob<'_>, under: &Under, rules: &RuleSet, counts: &mut LintCounts) -> Result<FileLinted, Why> {
    let refused = |what: &'static str| Why::Refused { what, rule: RULE };
    let frame = cx.frame();
    let path = core::str::from_utf8(job.text.path).map_err(|_| refused("its file's path is not UTF-8"))?;
    let at = Path::new(path).map_err(|_: bstd::path::PathError| refused("its file's path is no path"))?;
    let mut bytes = bstd::mem::Bytes::new_in(frame, "a source file, for its lint units");
    root.read_all(cx, &at, &mut bytes).map_err(|_| refused("its file could not be read"))?;
    counts.files_read += 1;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| refused("its file is not UTF-8"))?;
    if edition_source(ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()), job.text.edition) != job.text.source {
        return Err(Why::Changed);
    }
    lint_text(cx, text, path, job.items, job.task.planned.name, &job.units, job.prior, under, rules, counts)
}

/// File `path`'s lint task `name` over its `text` (checked against its outline) and its
/// planner's outline record's payload `items` (a FileItems record, read in place: the file is not
/// outlined again): each of its `units` kept from `prior` by its key ([`Prior`]) or linted
/// (lints.rs `lint_units_in`, split as `lints::Splitting` says), and the file's record made
/// ([`file_record`]). A unit that fails fails the file's task, by its why.
#[expect(clippy::too_many_arguments, reason = "one file's facts, each named; its callers are the job and its test")]
pub(crate) fn lint_text(
    cx: &Cx,
    text: &str,
    path: &str,
    items: &[u8],
    name: Name,
    units: &[UnitPart],
    prior: Option<&Prior>,
    under: &Under,
    rules: &RuleSet,
    counts: &mut LintCounts,
) -> Result<FileLinted, Why> {
    let refused = |what: &'static str| Why::Refused { what, rule: RULE };
    let view = blaze_surface::FileItemsView::read(items).map_err(|_| refused("its file's outline record holds no FileItems payload"))?;
    let erased = blaze_own::Erased::new(cx.frame());
    let alloc = erased.handle();
    let mut entries: blaze_own::AVec<'_, blaze_surface::Item> = blaze_own::AVec::with_capacity_in(view.len(), alloc);
    entries.extend(view.items().map(|one| one.item));
    let chain_of = |at: usize| work::stored_item_chain(view, blaze_surface::ItemIndex(at as u32));
    // 1. Each unit kept by its key, or asked.
    let mut found: std::vec::Vec<Found, Global> = std::vec::Vec::new_in(Global);
    let mut asked: std::vec::Vec<AskedUnit, Global> = std::vec::Vec::new_in(Global);
    for unit in units.iter() {
        match prior.and_then(|prior| prior.kept(under, unit.name, unit.key)) {
            Some(Some(kept)) => found.push(kept.clone()),
            Some(None) => {}
            None => asked.push((unit.name, unit.place)),
        }
    }
    // 2. The asked units linted. The same marks the outline's parts were keyed with (work.rs
    // `outline_file`): the file stands as its own crate root, as there.
    let subject = crate::signature::file_subject(path);
    let marks = crate::lints::marks_of(text, text);
    let mut did: blaze_own::Vec<Result<(usize, Record<Global>), Why>, Global> = blaze_own::Vec::with_capacity_in(asked.len(), Global);
    crate::lints::lint_units_in::<crate::lints::Splitting>(cx, &entries, &chain_of, text, path, subject, &asked, rules, marks, alloc, counts, &mut did).map_err(refused)?;
    // 3. Each linted unit's findings, its item named for the profile and the tests.
    let mut relinted = std::vec::Vec::with_capacity_in(did.len(), Global);
    let mut cost = (0u64, Cost::NONE);
    for one in did {
        let (unit, record) = one?;
        let entry = unit.checked_sub(1).and_then(|at| entries.get(at).map(|item| (at, item)));
        relinted.push(entry.map(|(_, item)| blaze_own::String::from_str_in(crate::item_name::shown_name(item, text), Global)));
        let payload = record.payload_bytes().map_err(|_| refused("a lint unit's record holds no payload"))?;
        let (tokens, sites) = crate::lints::cost_of(payload).ok_or(refused("a lint unit's record holds no cost"))?;
        cost.0 += u64::from(tokens);
        cost.1.add_all(&sites);
        if record.findings.is_empty() {
            continue;
        }
        let fixes = crate::lints::fixes_of(payload).ok_or(refused("a lint unit's record holds no findings' fixes"))?;
        let mut one = Found { name: record.name, chain: entry.map(|(at, _)| chain_of(at)), findings: std::vec::Vec::with_capacity_in(record.findings.len(), Global), words: std::vec::Vec::new_in(Global), fixes: std::vec::Vec::new_in(Global) };
        one.findings.extend(record.findings.iter().copied());
        one.words.extend(record.messages.iter().copied());
        one.fixes.extend_from_slice(fixes);
        found.push(one);
    }
    // 4. The file's record, its found units in name order (kept or linted, the same bytes).
    found.sort_unstable_by_key(|one| one.name);
    let (record, placed) = file_record(name, under, units, &found).map_err(refused)?;
    Ok(FileLinted { record, placed, relinted, units: units.len(), cost })
}

/// Part `name`'s place in its file, as the file's outline lists its `Lint` part: the parts are
/// sorted by name (work.rs `outline_file`), so one search. `None`: no such part.
fn part_place(parts: &[Part], name: Name) -> Option<u32> {
    let at = parts.binary_search_by_key(&name, |part| part.name).ok()?;
    matches!(parts[at].locate, Locate::Source(_)).then_some(parts[at].place)
}
