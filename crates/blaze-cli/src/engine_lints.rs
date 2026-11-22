//! The `Lints` stage of `blaze engine-check` (docs/v2/plan/design/lints-stage.md, D135): ONE
//! task an item, every selected token rule in its one walk, so an edit relints what it changed.
//!
//! THE PLAN, from text: one `Lint` task a lint unit an outline lists (lints.rs `parts_in`: unit 0 the
//! file with its top-level items as holes, one unit an outline entry with its children as holes),
//! in `Phase { stage: Lints, wave: 0 }`, named as the outline names it (its file and its entry's
//! chain), its source the unit's (its own bytes, holes, scope and `variant-count` read) with the
//! selected rule set's id and the edition folded in ([`plan`]). The files outlined are the
//! members' (found forward from their crate roots), so only workspace members are linted.
//!
//! THE TASK, on the pool: one job a file, its text read once and checked against the id its
//! outline's key was made from, outlined once, and its entered units linted by blaze-lint in one
//! pass each (lints.rs `lint_units_in`; blaze-lint lib.rs: one lex, each token dispatched to the
//! rules registered for its kind). Its row: `WHOLE` defined at the record's out (a digest of the
//! findings and their words, never of the cost), no read, its findings' count. Its record: the
//! findings as drive findings (rule number, item-relative range, words) and a payload of the
//! engine's findings with their fixes, then the pass's deterministic cost (lints.rs `COST_BYTES`).
//!
//! THE LINES: each finding placed in its item by the item's chain (a placed finding, as a body's)
//! or, for the file's unit, from the file's start; no level is stored (rule 81): the printer gives
//! each the level `shown.lints` says.

use core::convert::Infallible;
use core::num::NonZeroU32;

use blaze_drive::{Kind, Locate, Name, TaskFailure, Why};
use blaze_engine::{Buffer, Done, Phase, Planned, SVec, Stage, Task};
use blaze_ids::ContentId;
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
use crate::work::{self, Record};

/// A file's lint job's frame: its text, its outline, its units' scopes and findings.
const LINTS_FRAME: usize = 1 << 28;

const SOURCE_DOMAIN: &str = "blaze-cli engine lint task source 1";

/// What refuses a lint task here (rule 19).
const RULE: &str = "blaze-cli engine_lints.rs";

/// One `Lint` task a lint unit of each outline (sorted by name), whatever the step's rule set:
/// the set a unit runs is the step's joined with what its file's marks add (lints.rs
/// `lint_units_in`, blaze-lint `RootMarks::rules`), so an empty step set is no reason to skip a
/// file that denies a lint or is a sans-IO core. The marks are not in the outline's record, so
/// the plan cannot tell those files apart here; an unmarked file under an empty set makes empty
/// records, kept by their keys after the first run.
/// Its source (rule 21): the unit's (lints.rs `unit_source`, which holds the marks' key), the
/// rule set's id (a change of the step or of `lints-add` relints every unit and nothing else)
/// and the edition.
pub(crate) fn plan<'h>(outlined: &[Outlined<'_>], rules: &RuleSet, alloc: SharedAlloc<'h>) -> SVec<'h, Planned> {
    let mut planned = SVec::new_in(alloc);
    let set = rules.id();
    for one in outlined.iter() {
        let Ok(record) = &one.made else { continue };
        for part in record.parts.iter() {
            let (Kind::Lint, Locate::Source(unit)) = (part.name.kind, part.locate) else { continue };
            let mut h = blaze_ids::Hasher::new(SOURCE_DOMAIN);
            h.update(&unit.0).update(&set);
            planned.push(Planned { name: part.name, phase: Phase::new(Stage::Lints, 0), source: edition_source(h.finish(), one.edition), file: one.name });
        }
    }
    planned.sort_unstable_by_key(|one| one.name);
    planned.dedup_by_key(|one| one.name);
    planned
}

/// One unit as its task made it, all in `Global` (it crosses to the coordinator).
pub(crate) struct Made {
    pub(crate) name: Name,
    /// Its file, by its place in the planner's outlines.
    file: usize,
    /// Its item's chain (work.rs `item_chain`); `None` for the file's unit.
    chain: Option<u64>,
    /// Its item's name as written; `None` for the file's unit.
    item: Option<blaze_own::String<Global>>,
    pub(crate) record: Record<Global>,
}

/// One unit of a file's job, as a worker hands it back.
struct Unit {
    made: Result<Record<Global>, Why>,
    chain: Option<u64>,
    item: Option<blaze_own::String<Global>>,
}

/// One file's job, made on the coordinator: its outline's place and its entered tasks, each with
/// (task name, unit name, unit source) as lints.rs `lint_units_in` asks.
/// What the worker needs of the outline is copied out (its path, source and edition), so the job
/// crosses to a worker with no part of the outline's record.
struct FileJob<'p> {
    file: usize,
    text: FileText<'p>,
    tasks: std::vec::Vec<Task, Global>,
    asked: std::vec::Vec<(Name, Name, ContentId), Global>,
}

/// What one request's `Lints` stage saw, for the command's tests: each entered unit as
/// `path::item` (`path` alone for a file's unit), sorted, and the findings it made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LintsSeen {
    pub(crate) entered: std::vec::Vec<std::string::String, Global>,
    pub(crate) findings: u32,
}

/// The stage: the selected rules, each task's result and refusal.
pub(crate) struct LintsStage {
    rules: RuleSet,
    /// Each task entered and made, sorted by name after its phase.
    made: std::vec::Vec<Made, Global>,
    /// Each task refused, and why (rule 19).
    refused: std::vec::Vec<(Name, Why), Global>,
}

impl LintsStage {
    pub(crate) fn new(rules: RuleSet) -> LintsStage {
        LintsStage { rules, made: std::vec::Vec::new_in(Global), refused: std::vec::Vec::new_in(Global) }
    }

    /// Task `name`'s result, when it was entered and made this request.
    pub(crate) fn result(&self, name: Name) -> Option<&Made> {
        self.made.binary_search_by_key(&name, |one| one.name).ok().map(|at| &self.made[at])
    }

    /// The Lints phase: each file's entered units linted at once on the workers, one job a file,
    /// each task's row in `buffer`.
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
        let refused = &mut self.refused;
        let mut refuse = |task: &Task, why: Why, buffer: &mut Buffer<'a>| {
            buffer.done.push(crate::engine_tasks::failed(task, alloc));
            refused.push((task.planned.name, why));
        };
        // 1. Each task to its file's job, its unit's source from the outline that planned it.
        let mut jobs: std::vec::Vec<FileJob<'_>, Global> = std::vec::Vec::new_in(Global);
        for task in tasks.iter() {
            let Ok(file) = outlined.binary_search_by(|one| one.name.cmp(&task.planned.file)) else {
                refuse(task, Why::Refused { what: "a `Lint` task whose file is none of the planner's outlines", rule: RULE }, buffer);
                continue;
            };
            let unit = outlined[file].made.as_ref().ok().and_then(|record| {
                record.parts.iter().find_map(|part| match (part.name == task.planned.name, part.locate) {
                    (true, Locate::Source(source)) => Some(source),
                    _ => None,
                })
            });
            let Some(unit) = unit else {
                refuse(task, Why::Refused { what: "a `Lint` task its file's outline lists no unit of", rule: RULE }, buffer);
                continue;
            };
            if jobs.last().is_none_or(|job| job.file != file) {
                let one = &outlined[file];
                jobs.push(FileJob { file, text: FileText { path: &one.path[..], source: one.source, edition: one.edition }, tasks: std::vec::Vec::new_in(Global), asked: std::vec::Vec::new_in(Global) });
            }
            if let Some(job) = jobs.last_mut() {
                job.tasks.push(*task);
                job.asked.push((task.planned.name, task.planned.name, unit));
            }
        }
        // The tasks come sorted by name, a file's units apart: one job a file, merged.
        jobs.sort_by_key(|job| job.file);
        let mut merged: std::vec::Vec<FileJob<'_>, Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
        for job in jobs.into_iter() {
            match merged.last_mut() {
                Some(last) if last.file == job.file => {
                    last.tasks.extend(job.tasks);
                    last.asked.extend(job.asked);
                }
                _ => merged.push(job),
            }
        }
        let jobs = merged;
        // 2. Each file's units, at once, on the workers.
        let rules = &self.rules;
        type Ran = Result<Result<std::vec::Vec<Unit, Global>, Why>, work::Panicked>;
        let mut worked: std::vec::Vec<(Ran, u64), Global> = std::vec::Vec::with_capacity_in(jobs.len(), Global);
        {
            let scope = cx.scope();
            scope.narrow_workers(workers, Class::Foreground);
            let mut running = std::vec::Vec::with_capacity_in(jobs.len(), Global);
            for job in jobs.iter() {
                let clock = clock.clone();
                running.push(scope.may_overlap("engine lints", LINTS_FRAME, move |cx: &Cx| {
                    let began = clock.now();
                    let made = work::unwinding(|| lint_file(cx, root, job.text, &job.asked, rules));
                    Ok::<_, Infallible>((made, clock.now().since(began).map_or(0, crate::engine_times::ns)))
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
            let each = u64::try_from(job.tasks.len()).unwrap_or(1).max(1);
            for task in job.tasks.iter() {
                times.push(task.planned.name, ns / each);
            }
            let units = match ran {
                Ok(Ok(units)) => units,
                Ok(Err(why)) => {
                    for task in job.tasks.iter() {
                        refuse(task, why, buffer);
                    }
                    continue;
                }
                Err(work::Panicked) => {
                    for task in job.tasks.iter() {
                        refuse(task, Why::Refused { what: "its file's lint job panicked; that file's lint units alone failed", rule: RULE }, buffer);
                    }
                    continue;
                }
            };
            for (task, unit) in job.tasks.iter().zip(units) {
                match unit_done(task, job.file, unit, alloc) {
                    Ok((done, made)) => {
                        buffer.done.push(done);
                        self.made.push(made);
                    }
                    Err(why) => refuse(task, why, buffer),
                }
            }
        }
        self.made.sort_unstable_by_key(|one| one.name);
        Ok(())
    }

    /// Unit `name`'s block of the request's lines (engine_lines.rs): each finding by its start,
    /// end and rule (rule 25), placed in its item by the item's chain, or from the file's start for
    /// the file's unit; a refused unit's note. Nothing when it was not entered. A finding whose
    /// edition is not known is a typed stop naming the unit, never dropped (rule 3).
    pub(crate) fn lines(&self, name: Name, outlined: &[Outlined<'_>], lines: &mut Lines<'_>, frame: &Frame) -> Result<(), EngineCheckError> {
        if let Some(one) = self.result(name) {
            let file = outlined.get(one.file).ok_or(EngineCheckError::Unplaced { body: name, what: "its file is none of the planner's outlines" })?;
            let edition = blaze_surface::Edition::from_byte(file.edition).ok_or(EngineCheckError::Unplaced { body: name, what: "its file's edition byte is no edition" })?;
            let record = &one.record;
            let mut order: std::vec::Vec<usize, Global> = std::vec::Vec::with_capacity_in(record.findings.len(), Global);
            order.extend(0..record.findings.len());
            order.sort_unstable_by_key(|at| {
                let (rule, start, end, _, _) = record.findings[*at];
                (start, end, rule)
            });
            for at in order {
                let (rule, start, end, from, to) = record.findings[at];
                let words = &record.messages[from..to];
                match one.chain {
                    Some(chain) => lines.placed(name, rule, start, end, &file.path, chain, edition, words),
                    None => lines.finding(name, rule, start, end, Some(&file.path), words),
                }
            }
            return Ok(());
        }
        if let Some((_, why)) = self.refused.iter().find(|(refused, _)| *refused == name) {
            let failure = TaskFailure { name, why: *why };
            lines.note(name, crate::engine_lines::Class::Failure, bstd::mem::format_in!(frame, "a lint unit failed: {failure}").as_bytes());
        }
        Ok(())
    }

    /// The profile's lines: the tasks entered and their findings, then each rule that tried a
    /// site or found something, by its id, with its cost (the sites it tried, deterministic) and
    /// its findings; then the tokens the one lex made, the cost every rule shares.
    pub(crate) fn profile(&self, into: &mut blaze_own::String<&Frame>) {
        use core::fmt::Write as _;
        let mut cost = Cost::NONE;
        let mut tokens = 0u64;
        let mut found = [0u32; Rule::COUNT];
        for one in self.made.iter() {
            if let Some((lexed, sites)) = crate::lints::cost_of(&one.record.payload) {
                tokens = tokens.saturating_add(u64::from(lexed));
                cost.add_all(&sites);
            }
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
    }

    /// What the command's tests read ([`LintsSeen`]).
    pub(crate) fn seen(&self, outlined: &[Outlined<'_>]) -> LintsSeen {
        let mut entered = std::vec::Vec::with_capacity_in(self.made.len(), Global);
        let mut findings = 0u32;
        for one in self.made.iter() {
            let path = outlined.get(one.file).and_then(|file| core::str::from_utf8(&file.path).ok()).unwrap_or("?");
            entered.push(match &one.item {
                Some(item) => std::string::String::from(path) + "::" + item.as_str(),
                None => std::string::String::from(path),
            });
            findings = findings.saturating_add(u32::try_from(one.record.findings.len()).unwrap_or(u32::MAX));
        }
        entered.sort_unstable();
        LintsSeen { entered, findings }
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

/// One unit's row (`WHOLE` defined at the record's out, no read, its findings' count) and what
/// the request keeps of it: the one conversion from a worker's [`Unit`] to the engine's `Done`,
/// for the phase path's join and the `Ready` driver's [`LintsStage::lint_units`] alike (rule 30).
/// A unit that failed is its `Why`; the caller files the task's failed row.
fn unit_done<'a>(task: &Task, file: usize, unit: Unit, alloc: SharedAlloc<'a>) -> Result<(Done<'a>, Made), Why> {
    let record = unit.made?;
    let name = task.planned.name;
    let mut defines = SVec::with_capacity_in(1, alloc);
    defines.push((WHOLE, record.out));
    let findings = u32::try_from(record.findings.len()).unwrap_or(u32::MAX);
    let done = Done { name, key: task.key, out: record.out, defines, reads: SVec::new_in(alloc), findings, failed: false };
    Ok((done, Made { name, file, chain: unit.chain, item: unit.item, record }))
}

/// What the request keeps of one unit the `Ready` driver ran ([`LintsStage::lint_units`]): its
/// result, or its refusal and why, merged into the stage after the drive by [`LintsStage::keep`]
/// (a run reads the stage, never writes it: it runs on any worker).
pub(crate) enum UnitKept {
    Made(Made),
    Refused(Name, Why),
}

impl LintsStage {
    /// A file's entered `Lint` tasks in ONE walk, for the `Ready` driver (D187): the file's text
    /// read and checked as a phase's job does, its units walked once for their scopes (lints.rs
    /// `lint_units_in`; a unit's scope comes from its parents, so a walk a unit would read the
    /// file once a unit), the rules run on each asked unit, and each task's row with what the
    /// request keeps of it, in `asked`'s order. `file` is the outline's place in the planner's
    /// outlines; `asked` each task with its unit's source as the outline lists it. A failure of
    /// the whole file (its text moved, a panic) is each task's failed row, named.
    pub(crate) fn lint_units<'a>(&self, cx: &Cx, root: &Dir<ReadWrite>, file: usize, one: &Outlined<'_>, asked: &[(Task, ContentId)], alloc: SharedAlloc<'a>) -> SVec<'a, (Done<'a>, UnitKept)> {
        let text = FileText { path: &one.path[..], source: one.source, edition: one.edition };
        let mut units: SVec<'a, (Name, Name, ContentId)> = SVec::with_capacity_in(asked.len(), alloc);
        units.extend(asked.iter().map(|(task, unit)| (task.planned.name, task.planned.name, *unit)));
        let mut rows: SVec<'a, (Done<'a>, UnitKept)> = SVec::with_capacity_in(asked.len(), alloc);
        let refused = |task: &Task, why: Why| (crate::engine_tasks::failed(task, alloc), UnitKept::Refused(task.planned.name, why));
        match work::unwinding(|| lint_file(cx, root, text, &units, &self.rules)) {
            Ok(Ok(made)) => {
                // `lint_file` answers each asked unit once, in order: a missing answer is named.
                let mut made = made.into_iter();
                for (task, _) in asked.iter() {
                    rows.push(match made.next().map(|unit| unit_done(task, file, unit, alloc)) {
                        Some(Ok((done, kept))) => (done, UnitKept::Made(kept)),
                        Some(Err(why)) => refused(task, why),
                        None => refused(task, Why::Refused { what: "a `Lint` task its file's walk did not answer", rule: RULE }),
                    });
                }
            }
            Ok(Err(why)) => rows.extend(asked.iter().map(|(task, _)| refused(task, why))),
            Err(work::Panicked) => rows.extend(asked.iter().map(|(task, _)| refused(task, Why::Refused { what: "its file's lint walk panicked; that file's lint units alone failed", rule: RULE }))),
        }
        rows
    }

    /// What a `Ready` run of a unit made, kept as the phase path keeps it: a result in name order
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
/// the id its outline was made from (a difference is [`Why::Changed`]: no re-read, rule 3),
/// outlined once, its asked units linted (lints.rs `lint_units_in`), and each unit's item found by
/// its name for its chain.
fn lint_file(cx: &Cx, root: &Dir<ReadWrite>, job: FileText<'_>, asked: &[(Name, Name, ContentId)], rules: &RuleSet) -> Result<std::vec::Vec<Unit, Global>, Why> {
    let refused = |what: &'static str| Why::Refused { what, rule: RULE };
    let frame = cx.frame();
    let path = core::str::from_utf8(job.path).map_err(|_| refused("its file's path is not UTF-8"))?;
    let at = Path::new(path).map_err(|_: bstd::path::PathError| refused("its file's path is no path"))?;
    let mut bytes = bstd::mem::Bytes::new_in(frame, "a source file, for its lint units");
    root.read_all(cx, &at, &mut bytes).map_err(|_| refused("its file could not be read"))?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| refused("its file is not UTF-8"))?;
    if edition_source(ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()), job.edition) != job.source {
        return Err(Why::Changed);
    }
    let edition = blaze_surface::Edition::from_byte(job.edition).ok_or(refused("its file's edition byte is no edition"))?;
    let erased = blaze_own::Erased::new(frame);
    let outline = blaze_file_items::outline(text, edition, erased.handle());
    let subject = crate::signature::file_subject(path);
    let mut did: blaze_own::Vec<Option<Result<Record<Global>, Why>>, Global> = blaze_own::Vec::with_capacity_in(asked.len(), Global);
    // The same marks the outline's parts were keyed with (work.rs `outline_file`): the file
    // stands as its own crate root, as there.
    let marks = crate::lints::marks_of(text, text);
    crate::lints::lint_units_in(&outline, text, path, subject, asked, rules, marks, erased.handle(), &mut did).map_err(refused)?;
    // Each unit by its name (unit 0 the file, unit `i + 1` entry `i`), searched once a task.
    let items = outline.items();
    let mut names: std::vec::Vec<(Name, usize), Global> = std::vec::Vec::with_capacity_in(items.len() + 1, Global);
    names.extend((0..=items.len()).map(|at| (crate::lints::unit_name(subject, &outline, at, text), at)));
    names.sort_unstable();
    let mut units = std::vec::Vec::with_capacity_in(asked.len(), Global);
    for ((name, _, _), made) in asked.iter().zip(did) {
        let entry = names.binary_search_by_key(name, |(unit, _)| *unit).ok().and_then(|at| names[at].1.checked_sub(1)).map(|at| &items[at]);
        let chain = entry.map(|item| work::item_chain(&outline, item, text));
        let item = entry.map(|item| blaze_own::String::from_str_in(crate::item_name::shown_name(item, text), Global));
        let made = made.unwrap_or(Err(refused("a `Lint` task its file's units do not answer")));
        units.push(Unit { made, chain, item });
    }
    Ok(units)
}
