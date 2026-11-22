//! The reader index's two strategies held equal (rule 78), its planted bug caught (rule 23), and
//! its cost at scale against the floor (rule 67). readers.rs says what each strategy is.
//!
//! - `the_column_index_decides_as_the_maps`: sim.rs's histories (its world, edits, scopes, pools,
//!   poisoned bodies and growth) run twice, request by request, once on [`Mapped`] (the index as
//!   it was) and once on [`Paired`] (the columns): every phase opened, with its tasks in order,
//!   every entered task and its cause, the report (but the index's cost), every table of the
//!   answer and the stored answer's bytes (its pages' root) are equal.
//! - `control_a_dropped_last_reader_is_caught`: the same with `Plant::LastReaderDropped` in the
//!   columns alone (a question's answer loses the cell's last reader): the test must fail.
//! - `scale_twenty_thousand_tasks_of_five_hundred_reads` (ignored: about 2 GB; run it in release):
//!   both strategies fed a Heads-like stage's writes and questions, their cost printed.

use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;
use std::{eprintln, println};

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::engine::{Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Everything, Finished, Phase, Plant, Planned, Pool, Report, SVec, Stage, Task, Tools, drive};
use crate::pages::Kept;
use crate::readers::{FLOOR_BYTES_PER_EDGE, IndexCost, Mapped, Paired, ReaderIndex};
use crate::sim::{Crates, POISON, Pools, World, alloc, edit, first_world, grow_names, oracle, work};

/// The words every oracle of the differential test says: the control waits for them.
const DIFFER: &str = "the strategies differ";

/// The tool the requests key their tasks by: any, the same on both sides.
const TOOL: Tool = Tool(ContentId([7; 32]));

/// Each phase a request opened and the tasks it entered, in order.
type Steps = StdVec<(Phase, StdVec<Name>)>;
type Memberships = BTreeSet<(CellId, Name)>;
type Delta = StdVec<((CellId, Name), bool)>;

fn forward_memberships(answer: &Answer<'_>) -> Memberships {
    answer.rows.iter().flat_map(|(&reader, row)| row.reads.iter().map(move |&(id, _)| (id, reader))).collect()
}

/// One request on the strategy `R`: the phases it opened, its tasks each, and how it finished.
fn request<R: ReaderIndex<'static>>(
    world: &World,
    answer: Answer<'static>,
    changed: &BTreeMap<Name, StdVec<Planned>>,
    scope: Crates,
    plant: Plant,
    mut pool: Pools,
) -> Result<(Finished<'static>, Steps, Delta), EngineError> {
    let files: StdVec<(Name, &[Planned])> = changed.iter().map(|(name, plan)| (*name, plan.as_slice())).collect();
    let mut engine: Engine<'static, Crates, Answer<'static>, R> = Engine::with_index_in(Tools([TOOL; key::KINDS]), answer, &Edit { files: &files }, scope, plant, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = (0..4).map(|_| Buffer::new_in(alloc())).collect();
    let mut steps: Steps = StdVec::new();
    while let Some(phase) = engine.next_phase()? {
        steps.push((phase, engine.tasks().iter().map(|task| task.planned.name).collect()));
        {
            let view = engine.view();
            let tasks = engine.tasks();
            // The tasks get no plant: the strategies' tasks are the same.
            pool.run(tasks, &mut buffers, &|_: &(), task: &Task, buffer: &mut Buffer<'static>| work(world, Plant::None, view, task, buffer));
        }
        engine.merge(&mut buffers)?;
        grow_names(world, &mut engine)?;
    }
    let mut changes = Delta::new();
    let finished = engine.finish_with_readers(|delta| {
        changes.extend_from_slice(&delta.edits);
        Ok(())
    })?;
    Ok((finished, steps, changes))
}

/// The report as a decision: everything but what the index cost.
fn decided(report: &Report) -> Report {
    Report { index: IndexCost::default(), ..*report }
}

/// The stored answer's root: its bytes, as pages.rs encodes them.
fn stored(answer: Answer<'static>, finished: &[crate::engine::Entered]) -> (Answer<'static>, blaze_drive::key::Key) {
    let kept = Kept::after(blaze_own::BTreeMap::new_in(alloc()), answer, finished, blaze_own::BTreeMap::new_in(alloc()));
    let root = crate::pages::encode(&kept, None, alloc()).root;
    (kept.answer, root)
}

/// One history on both strategies, held equal request by request. `plant` goes to the columns'
/// side alone.
fn differential(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = first_world(tape);
    let everything: BTreeSet<usize> = (0..world.crates.len()).collect();
    let crate_of = Crates::of(&world, BTreeSet::new()).crate_of;
    let mut old: Answer<'static> = Answer::new_in(alloc());
    let mut new: Answer<'static> = Answer::new_in(alloc());
    let mut last_plans: BTreeMap<Name, StdVec<Planned>> = BTreeMap::new();
    for serial in 0..2 + tape.below(9, "requests") {
        if serial > 0 {
            world = edit(tape, &world);
        }
        let asked: BTreeSet<usize> = match tape.one_in(2, "a narrower scope") {
            true => everything.iter().copied().filter(|_| tape.one_in(2, "a crate asked")).collect(),
            false => everything.clone(),
        };
        if tape.one_in(4, "a body poisoned") {
            let count: usize = world.files.iter().map(|(_, items)| items.len()).sum();
            if count > 0 {
                let at = tape.index(count, "the poisoned body");
                if let Some(item) = world.files.iter_mut().flat_map(|(_, items)| items.iter_mut()).nth(at) {
                    item.body = POISON;
                }
            }
        }
        let pool = [Pools::One, Pools::Many(1 + tape.index(4, "workers")), Pools::Reversed][tape.index(3, "the pool")];
        let scope = Crates { asked, crate_of: crate_of.clone() };
        let plans = world.plans();
        let changed: BTreeMap<Name, StdVec<Planned>> =
            plans.iter().filter(|(name, plan)| last_plans.get(*name) != Some(*plan)).map(|(name, plan)| (*name, plan.clone())).collect();
        let before = forward_memberships(&old);
        let (was, was_steps, was_delta) = request::<Mapped<'static>>(&world, old, &changed, scope.clone(), Plant::None, pool)
            .map_err(|error| Broke::Harness(format!("request {serial}: the old index refused: {error}")))?;
        let (now, now_steps, now_delta) = request::<Paired<'static>>(&world, new, &changed, scope, plant, pool)
            .map_err(|error| Broke::Oracle(format!("request {serial}: {DIFFER}: the columns refused what the maps took: {error}")))?;
        last_plans = plans;
        // Every phase, its tasks in order (a wave's entered tasks in key order, rule 25).
        oracle(was_steps.len() == now_steps.len(), || format!("request {serial}: {DIFFER}: {} phases on the maps, {} on the columns", was_steps.len(), now_steps.len()))?;
        for (at, (a, b)) in was_steps.iter().zip(now_steps.iter()).enumerate() {
            oracle(a == b, || format!("request {serial}: {DIFFER} at phase {at}: maps {a:?}, columns {b:?}"))?;
        }
        oracle(was.entered[..] == now.entered[..], || format!("request {serial}: {DIFFER} in what was entered"))?;
        oracle(was.causes[..] == now.causes[..], || format!("request {serial}: {DIFFER} in the causes"))?;
        oracle(decided(&was.report) == decided(&now.report), || format!("request {serial}: {DIFFER} in the report: maps {:?}, columns {:?}", was.report, now.report))?;
        let (a, b) = (&was.answer, &now.answer);
        let after = forward_memberships(a);
        let want: Delta = before.symmetric_difference(&after).map(|pair| (*pair, after.contains(pair))).collect();
        oracle(was_delta == want && now_delta == want, || format!("request {serial}: {DIFFER} from the forward membership difference"))?;
        for (table, equal) in [
            ("rows", a.rows == b.rows),
            ("cells", a.cells == b.cells),
            ("reverse index", a.readers == b.readers),
            ("files", a.by_file == b.by_file),
            ("owed cells", a.owed_cells == b.owed_cells),
            ("owed tasks", a.owed_tasks == b.owed_tasks),
            ("rows with findings", a.with_findings == b.with_findings),
            ("serial", a.serial == b.serial),
        ] {
            oracle(equal, || format!("request {serial}: {DIFFER} in the answer's {table}"))?;
        }
        let (was_answer, was_root) = stored(was.answer, &was.causes);
        let (now_answer, now_root) = stored(now.answer, &now.causes);
        oracle(was_root == now_root, || format!("request {serial}: {DIFFER} in the stored answer's bytes: root {was_root:?} on the maps, {now_root:?} on the columns"))?;
        // The columns' own counts are exact and the maps' lower bounds (readers.rs `IndexCost`).
        oracle(now.report.index.exact && !was.report.index.exact, || format!("request {serial}: the index costs say exact wrongly: {:?}", now.report.index))?;
        old = was_answer;
        new = now_answer;
    }
    Ok(())
}

/// Expected: "test readers_sim::the_column_index_decides_as_the_maps ... ok".
#[test]
fn the_column_index_decides_as_the_maps() {
    check(SimConfig::cases(300), |tape| differential(tape, Plant::None));
}

/// Expected: ok, and on standard error "plant LastReaderDropped: caught at case k of 300": the
/// columns losing a cell's last reader changes a decision or the answer, and the test says so.
#[test]
fn control_a_dropped_last_reader_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::LastReaderDropped, DIFFER, differential);
}

/// A name the scale test makes: `n` in the subject's first bytes.
fn scale_name(n: u32) -> Name {
    let mut subject = [0u8; 32];
    subject[..4].copy_from_slice(&n.to_le_bytes());
    Name::new(Kind::Code, ContentId(subject))
}

/// The `j`-th cell of the scale test's pool: one of `CELLS` cells of 1,000 definers.
fn scale_cell(j: u32) -> CellId {
    let mut cell = [0u8; 16];
    cell[..4].copy_from_slice(&j.to_le_bytes());
    CellId { of: scale_name(1_000_000 + j % 1_000), cell: Cell(cell) }
}

/// All rows re-enter with identical output. One swaps a selector in revision one.
/// Revision two moves the retired selector and one common selector, and reverses
/// observation order. Only a leaked tombstone can leave the retired selector owed.
fn membership_history(tape: &Tape, plant: Plant, rows: u32, common: u32) -> Result<(), Broke> {
    let owner = scale_name(rows);
    let file = scale_name(rows + 1);
    let special = tape.index(rows as usize, "reader changing a selector") as u32;
    let first = tape.index(common as usize, "first common observation") as u32;
    let selector = |n: u32| {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&n.to_be_bytes());
        CellId { of: owner, cell: Cell(bytes) }
    };
    let (removed, added) = (selector(common), selector(common + 1));
    let digest = ContentId([42; 32]);
    let mut answer = Answer::new_in(alloc());
    let mut edits = StdVec::new();
    for revision in 0u8..3 {
        let plan: StdVec<_> = (0..=rows).map(|n| {
            let is_owner = n == rows;
            let version = if is_owner { u8::from(revision == 2) } else { revision };
            Planned {
                name: scale_name(n),
                file,
                phase: Phase::new(if is_owner { Stage::Heads } else { Stage::Bodies }, 0),
                source: ContentId([version; 32]),
            }
        }).collect();
        let run = || -> Result<Finished<'static>, EngineError> {
            let files = [(file, plan.as_slice())];
            let mut engine = Engine::new_in(Tools([TOOL; key::KINDS]), answer, &Edit { files: &files }, Everything, plant, alloc())?;
            let mut buffers = [Buffer::new_in(alloc())];
            drive(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| {
                let mut defines = SVec::new_in(alloc());
                let mut reads = SVec::new_in(alloc());
                let name = task.planned.name;
                if name == owner {
                    if revision == 2 {
                        defines.push((selector(first).cell, digest));
                        defines.push((removed.cell, digest));
                    }
                } else {
                    for i in 0..common {
                        let id = selector((first + i) % common);
                        reads.push((id, view.cell(&id)));
                    }
                    if name == scale_name(special) {
                        let id = if revision == 0 { removed } else { added };
                        reads.push((id, view.cell(&id)));
                    }
                    if revision == 2 {
                        reads.reverse();
                    }
                }
                buffer.done.push(Done { name, key: task.key, out: digest, reads, defines, findings: 0, failed: false });
            })?;
            engine.finish_with_readers(|delta| {
                if revision == 1 {
                    edits.extend_from_slice(&delta.edits);
                }
                Ok(())
            })
        };
        let finished = run().map_err(|error| Broke::Harness(format!("membership revision {revision}: {error}")))?;
        let cost = finished.report.index;
        if revision > 0 {
            let want = if revision == 1 { 2 } else { 0 };
            oracle(cost.writes == want, || format!("membership write bound: revision {revision} wrote {}, expected {want}", cost.writes))?;
            oracle(cost.bytes <= 4096, || format!("membership capacity: revision {revision} reserved {} bytes", cost.bytes))?;
        }
        answer = finished.answer;
        if revision == 2 {
            oracle(answer.owed_cells.is_empty(), || format!("next edit owes a cell after every current reader saw it: {:?}", answer.owed_cells))?;
            let row = &answer.rows[&scale_name(special)];
            oracle(row.reads.first().map(|r| r.0) == Some(added) && row.reads.last() == Some(&(selector(first), digest)), || {
                String::from("the identical output must retain the new observation order and the moved selector's digest")
            })?;
        }
    }
    oracle(edits == [((removed, scale_name(special)), false), ((added, scale_name(special)), true)], || format!("unexpected membership delta {edits:?}"))
}

#[test]
fn one_swap_among_a_hundred_thousand_standing_memberships() {
    check(SimConfig::cases(2), |tape| membership_history(tape, Plant::None, 100, 1_000));
}

#[test]
fn control_rewriting_all_memberships_breaks_the_write_bound() {
    control(SimConfig::cases(4), Plant::None, Plant::RewriteAllMemberships, "membership write bound", |tape, plant| membership_history(tape, plant, 8, 12));
}

#[test]
fn control_a_lost_tombstone_leaves_the_next_edit_owed() {
    control(SimConfig::cases(4), Plant::None, Plant::LostReaderTombstone, "next edit owes a cell", |tape, plant| membership_history(tape, plant, 8, 12));
}

#[test]
fn membership_reversed_in_a_later_wave_is_absent_from_the_fold() {
    check(SimConfig::cases(4), |tape| -> Result<(), Broke> {
        let file = scale_name(70);
        let owner = scale_name(tape.index(2, "which name merges first") as u32);
        let reader = if owner == scale_name(0) { scale_name(1) } else { scale_name(0) };
        let trigger = CellId { of: owner, cell: Cell([0; 16]) };
        let mut answer = Answer::new_in(alloc());
        for revision in 1..=2u8 {
            let source = ContentId([revision; 32]);
            let plan = [owner, reader].map(|name| Planned { name, file, phase: Phase::new(Stage::Heads, 0), source });
            let files = [(file, &plan[..])];
            let mut delta = Delta::new();
            let run = || -> Result<Finished<'static>, EngineError> {
                let mut engine = Engine::new_in(Tools([TOOL; key::KINDS]), answer, &Edit { files: &files }, Everything, Plant::None, alloc())?;
                let mut buffers = [Buffer::new_in(alloc())];
                drive(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| {
                    let name = task.planned.name;
                    let (mut reads, mut defines) = (SVec::new_in(alloc()), SVec::new_in(alloc()));
                    if name == owner {
                        defines.push((trigger.cell, source));
                    } else {
                        let seen = view.cell(&trigger);
                        reads.push((trigger, seen));
                        // The first wave sees yesterday's trigger, the next sees today's.
                        let id = CellId { of: owner, cell: Cell([if seen == source { 1 } else { 2 }; 16]) };
                        reads.push((id, view.cell(&id)));
                    }
                    buffer.done.push(Done { name, key: task.key, out: ContentId([17; 32]), reads, defines, findings: 0, failed: false });
                })?;
                engine.finish_with_readers(|changes| {
                    delta.extend_from_slice(&changes.edits);
                    Ok(())
                })
            };
            let finished = run().map_err(|error| Broke::Harness(format!("wave revision {revision}: {error}")))?;
            oracle(finished.entered.iter().filter(|&&name| name == reader).count() == 2, || String::from("the moved trigger must enter its reader in the next wave"))?;
            if revision == 2 {
                oracle(finished.report.index.writes == 4 && delta.is_empty(), || format!("two opposite swaps should fold no edits: {delta:?}"))?;
            }
            answer = finished.answer;
        }
        Ok(())
    });
}

fn audit_oracle(answer: &Answer<'_>) -> Result<(), Broke> {
    let mut examples = StdVec::new();
    let (mut ordinary, mut signatures, mut heads) = (0u32, 0u32, 0u32);
    for (&reader, row) in answer.rows.iter() {
        heads += u32::from(row.phase.stage == Stage::Heads);
        for &(id, _) in row.reads.iter() {
            if answer.cells.contains_key(&id) || answer.rows.contains_key(&id.of) {
                continue;
            }
            if id.of.kind == Kind::Signature {
                signatures += 1;
            } else {
                ordinary += 1;
                if examples.len() < 8 {
                    examples.push((reader, id));
                }
            }
        }
    }
    let proof = answer.read_audit().map_err(|error| Broke::Oracle(format!("the fold lost its base audit: {error}")))?;
    oracle(proof.heads() == heads && proof.undefined_counts() == (ordinary, signatures) && proof.examples().eq(examples.iter().copied()), || {
        format!("audit differs from the forward rows: expected {heads} heads, {ordinary} undefined, {signatures} signatures, examples {examples:?}")
    })
}

fn audit_history(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let owner = scale_name(80);
    let signature = Name::new(Kind::Signature, owner.subject);
    let file = scale_name(81);
    let rows = 10 + tape.index(8, "rows that read absent selectors") as u32;
    let mut answer = Answer::new_in(alloc());
    for revision in 0u8..5 {
        let mut plan = StdVec::new();
        let mut plan_row = |name, stage, version| {
            plan.push(Planned { name, phase: Phase::new(stage, 0), file, source: ContentId([version; 32]) });
        };
        for n in 0..rows {
            // One reader retires permanently. The others re-enter only for the final
            // change of observation order, when the membership delta is empty.
            if n != 0 || revision < 2 {
                plan_row(scale_name(n), Stage::Bodies, u8::from(revision == 4));
            }
        }
        if revision == 1 {
            plan_row(owner, Stage::Heads, 0);
        }
        if revision == 2 || revision == 3 {
            plan_row(signature, Stage::Heads, 0);
        }
        let run = || -> Result<Finished<'static>, EngineError> {
            let files = [(file, plan.as_slice())];
            let mut engine = Engine::new_in(Tools([TOOL; key::KINDS]), answer, &Edit { files: &files }, Everything, plant, alloc())?;
            let mut buffers = [Buffer::new_in(alloc())];
            drive(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| {
                let name = task.planned.name;
                let mut reads = SVec::new_in(alloc());
                if name != owner && name != signature {
                    for of in [signature, owner, owner] {
                        let id = CellId { of, cell: Cell([reads.len() as u8; 16]) };
                        reads.push((id, view.cell(&id)));
                    }
                    if revision == 4 {
                        reads.reverse();
                    }
                }
                // A present row defines no selectors: reads of its absent cells are intentional.
                buffer.done.push(Done { name, key: task.key, out: ContentId([19; 32]), reads, defines: SVec::new_in(alloc()), findings: 0, failed: false });
            })?;
            engine.finish()
        };
        answer = run().map_err(|error| Broke::Harness(format!("audit revision {revision}: {error}")))?.answer;
        audit_oracle(&answer)?;
        if revision <= 1 {
            // Seed a fresh process from the current page format (which requires reads
            // in cell order). Compare the decoded proof with the forward oracle too.
            let kept = Kept::after(blaze_own::BTreeMap::new_in(alloc()), answer, &[], blaze_own::BTreeMap::new_in(alloc()));
            let bytes = crate::pages::encode(&kept, None, alloc());
            let restored = crate::pages::decode(bytes.root, |key| bytes.puts.iter().find(|p| p.0 == key).map(|p| crate::pages::Sealed::hashed(key, p.1.as_slice())), alloc())
                .map_err(|error| Broke::Harness(format!("restoring the audit fixture: {error}")))?;
            let encoded_again = crate::pages::encode(&restored, None, alloc());
            oracle(bytes.root == encoded_again.root, || String::from("audit fixture changed bytes on restore"))?;
            answer = restored.answer;
            if revision == 0 {
                let proof = kept.answer.read_audit().map_err(|error| Broke::Harness(format!("saving the audit: {error}")))?;
                let mut other_root = bytes.root;
                other_root.0.0[0] ^= 1;
                let stale = answer.restore_read_audit(bytes.root, proof.record(other_root), alloc());
                oracle(matches!(stale, Err(EngineError::AuditBase { .. })), || String::from("restore accepted a proof for a different root"))?;
                answer.restore_read_audit(bytes.root, proof.record(bytes.root), alloc())
                    .map_err(|error| Broke::Oracle(format!("restoring the matching audit: {error}")))?;
            } else {
                answer.certify_reads(alloc());
            }
            audit_oracle(&answer)?;
        }
    }
    Ok(())
}

#[test]
fn audit_matches_forward_rows_after_definer_changes_and_restore() {
    check(SimConfig::cases(8), |tape| audit_history(tape, Plant::None));
}

#[test]
fn control_skipping_readers_of_an_introduced_definer_is_caught() {
    control(SimConfig::cases(4), Plant::None, Plant::SkipDefinerReaders, "audit differs from the forward rows", audit_history);
}

const TASKS: u32 = 20_000;
const READS: u32 = 500;
const CELLS: u32 = 200_000;
/// Heads in waves: 40 phases of 500 tasks.
const PHASES: u32 = 40;
/// Questions a phase: one a task (its defined cell moved), each of a cell of the pool.
const QUESTIONS: u32 = TASKS / PHASES;

/// What one strategy held and cost after the scale test's stage, and the answers' total length
/// (equal for both, or the strategies differ).
fn scale<R: ReaderIndex<'static>>() -> (IndexCost, u64) {
    let mut index = R::new_in(alloc(), Plant::None);
    let mut out: SVec<'static, (Name, bool)> = blaze_own::Vec::new_in(alloc());
    let mut answered = 0u64;
    let mut phase_max = 0u64;
    let per = TASKS / PHASES;
    for phase in 0..PHASES {
        let before = index.cost().allocations;
        index.reserve((per * READS) as usize);
        for t in phase * per..(phase + 1) * per {
            let reader = scale_name(t);
            for r in 0..READS {
                // Distinct cells a task, spread over the pool.
                let j = (t.wrapping_mul(7_919).wrapping_add(r.wrapping_mul(104_729))) % CELLS;
                index.write(scale_cell(j), reader, true);
            }
        }
        for q in 0..QUESTIONS {
            index.edits_of(scale_cell((phase * QUESTIONS + q).wrapping_mul(31) % CELLS), &mut out);
            answered += out.len() as u64;
        }
        phase_max = phase_max.max(index.cost().allocations - before);
    }
    let mut cost = index.cost();
    cost.phase_allocations_max = phase_max;
    (cost, answered)
}

fn say(strategy: &str, cost: &IndexCost) -> String {
    let per_edge = cost.bytes_per_edge();
    let bound = match cost.exact {
        true => "exact",
        false => "at least",
    };
    format!(
        "{strategy}: {} edges, {} bytes ({bound}), {per_edge} bytes an edge (floor {FLOOR_BYTES_PER_EDGE}, ratio {:.2}), {} allocations ({bound}), at most {} in a phase, {} runs, {} seals",
        cost.edges,
        cost.bytes,
        per_edge as f64 / FLOOR_BYTES_PER_EDGE as f64,
        cost.allocations,
        cost.phase_allocations_max,
        cost.runs,
        cost.seals,
    )
}

/// Expected (run with `--release -- --ignored`): two lines, "columns: 10000000 edges, ...
/// ratio ~1.0x, ... at most N in a phase" with N at most `4 + log2(edges)` and "maps: ...
/// (at least) ...", and the answers' lengths equal.
#[test]
#[ignore = "about 2 GB and a minute: run in release with --ignored"]
fn scale_twenty_thousand_tasks_of_five_hundred_reads() {
    let (columns, columns_answered) = scale::<Paired<'static>>();
    println!("{}", say("columns", &columns));
    let (maps, maps_answered) = scale::<Mapped<'static>>();
    println!("{}", say("maps", &maps));
    eprintln!("answers: {columns_answered} readers on the columns, {maps_answered} on the maps");
    assert_eq!(columns_answered, maps_answered, "the strategies answer the questions alike");
    assert_eq!(columns.edges, u64::from(TASKS) * u64::from(READS), "the columns hold one entry an edge (each pair written once)");
    // The floor (rule 67): allocations a phase O(columns): the tail's one growth, the seal's
    // run, and the merges of its cascade (at most one a run held, log2(edges) + 1).
    let log2 = u64::from(64 - columns.edges.leading_zeros());
    assert!(columns.phase_allocations_max <= 4 + log2, "a phase made {} allocations, past 4 + log2(edges) = {}", columns.phase_allocations_max, 4 + log2);
    // Bytes an edge: the pair, its flag, and the tail's place, within 25% of the floor at the end
    // (the merged runs hold one entry an edge; the tail keeps a phase's capacity).
    assert!(columns.bytes_per_edge() * 4 <= FLOOR_BYTES_PER_EDGE as u64 * 5 + 4 * 32, "{} bytes an edge against a floor of {FLOOR_BYTES_PER_EDGE}", columns.bytes_per_edge());
}
