//! A CRATE'S HEAD TASKS SPLIT OVER THE WORKERS (engine_heads.rs `split_count`, `heads_of`,
//! `in_split_order`, lane OW1). A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE FLOOR: busy jobs = min(workers, tasks with work). One crate of eight files, on eight
//! workers: under the rule its tasks are cut into more than one job and more than one ran a task.
//! EQUAL ANSWERS: the crate's tasks in one job against eight jobs, each on a fresh world of one
//! seed: equal findings, equal head rows (each `Signature` row's out, defines and read list, as
//! the stored answer holds them) and equal table counts (cells, frozen, bytes held).
//! WHOLE FILES: six files with 1, 1, 40, 3, 3 and 2 tasks. The large file stays in its own
//! job. The old evenly spaced cuts cross that file three times, counted as one shared file.
//! THE MERGE (rule 43, `blaze_sim`): the split jobs' results put in split order whatever order
//! they finished in, every order the tape draws. CONTROL (rule 23): the planted arrival order
//! (`MergePlant::Arrival`) is caught by the same oracle.

use super::engine_check_tests::{engine_opts, opts};
use super::*;

use crate::engine_heads::{HeadJobs, HeadSplit, MergePlant, file_splits, in_split_order};
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

const WORKSPACE: &str = "[workspace]\nmembers = [\"four\"]\nresolver = \"2\"\n";
const MANIFEST: &str = "[package]\nname = \"four\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const LIB: &str = "pub mod a;\npub mod b;\npub mod c;\n\npub struct Root {\n    pub a: a::A,\n    pub b: b::B,\n}\n\npub fn root(a: a::A, b: b::B) -> Root {\n    Root { a, b }\n}\n\npub fn total(r: &Root) -> u32 {\n    c::sum(&r.a, &r.b)\n}\n";
const A: &str = "pub struct A {\n    pub x: u32,\n}\n\npub struct A2 {\n    pub a: A,\n}\n\npub enum AE {\n    One(A),\n    Two(A2),\n}\n\npub fn a(x: u32) -> A {\n    A { x }\n}\n\npub fn a2(a: A) -> A2 {\n    A2 { a }\n}\n\npub fn ax(e: &AE) -> u32 {\n    match e {\n        AE::One(a) => a.x,\n        AE::Two(b) => b.a.x,\n    }\n}\n";
const B: &str = "use crate::a::A;\n\npub struct B {\n    pub a: A,\n    pub y: u32,\n}\n\npub trait Get {\n    fn get(&self) -> u32;\n}\n\nimpl Get for B {\n    fn get(&self) -> u32 {\n        self.a.x + self.y\n    }\n}\n\npub fn b(a: A, y: u32) -> B {\n    B { a, y }\n}\n\npub const TWO: u32 = 2;\n";
const C: &str = "use crate::a::A;\nuse crate::b::{B, Get, TWO};\n\npub struct C {\n    pub b: B,\n}\n\npub fn sum(a: &A, b: &B) -> u32 {\n    a.x + b.get() * TWO\n}\n\npub fn c(b: B) -> C {\n    C { b }\n}\n\npub fn cx(c: &C) -> u32 {\n    c.b.get()\n}\n";

fn eight_files(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "four/Cargo.toml", MANIFEST);
    write(&io, "four/src/lib.rs", &format!("{LIB}pub mod d;\npub mod e;\npub mod f;\npub mod g;\n"));
    write(&io, "four/src/a.rs", A);
    write(&io, "four/src/b.rs", B);
    write(&io, "four/src/c.rs", C);
    for module in ["d", "e", "f", "g"] {
        write(&io, &format!("four/src/{module}.rs"), &functions(5));
    }
    io
}

const UNEVEN: [usize; 6] = [1, 1, 40, 3, 3, 2];

fn uneven_files() -> Io {
    let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "four/Cargo.toml", MANIFEST);
    let mut paths = ["four/src/lib.rs", "four/src/a.rs", "four/src/b.rs", "four/src/c.rs", "four/src/d.rs", "four/src/e.rs"];
    paths.sort_unstable_by_key(|path| blaze_drive::Name::file(path.as_bytes()));
    for (path, tasks) in paths.into_iter().zip(UNEVEN) {
        let mut source = functions(tasks);
        if path == "four/src/lib.rs" {
            source.push_str("pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\n");
        }
        write(&io, path, &source);
    }
    io
}

/// What one cold request answered, by what this file compares.
struct Said {
    ids: std::vec::Vec<(std::string::String, std::string::String)>,
    checked: engine_check::Checked,
    /// The digest of every `Signature` row the stored answer holds (sorted by name), and how many.
    rows: (blaze_ids::ContentId, u32),
}

/// One cold request on eight workers, the crate's tasks split as `split` says.
fn request(io: &Io, split: HeadSplit) -> Said {
    let mut o = opts(io, 8, false);
    let g = o.heap.alloc();
    o.head_jobs = HeadJobs { split, ..HeadJobs::RULE };
    let (ids, checked) = engine_opts(io, o);
    Said { ids, checked, rows: head_rows(io, g) }
}

/// Every head row of the stored answer (engine_why.rs `with_kept`): each `Signature` row's out,
/// defines, read list (each read's cell and digest), findings and failure, hashed by name order.
fn head_rows(io: &Io, g: blaze_own::Alloc<'static>) -> (blaze_ids::ContentId, u32) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let err = io.need_err().expect("err");
    let mut phases = Phases::new(io.need_clock().expect("a clock"));
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let judged = crate::engine_why::with_kept(&cx, &fs, &host, &mut rng, &root, &opts(io, 2, false), &mut phases, &err, |kept| {
        let mut each: blaze_own::Vec<(blaze_drive::Name, blaze_ids::ContentId), blaze_own::Alloc<'static>> = blaze_own::Vec::new_in(g);
        for (name, row) in kept.answer.rows.iter().filter(|(name, _)| name.kind == blaze_drive::Kind::Signature) {
            let mut h = blaze_ids::Hasher::new("blaze-cli test: one head row");
            h.part(&row.out.0);
            for (cell, digest) in row.defines.iter() {
                h.part(&cell.0).part(&digest.0);
            }
            for (id, digest) in row.reads.iter() {
                h.part(&[id.of.kind as u8]).part(&id.of.subject.0).part(&id.cell.0).part(&digest.0);
            }
            h.part(&[u8::from(row.failed)]).part(&row.findings.to_le_bytes());
            each.push((*name, h.finish()));
        }
        each.sort_unstable_by_key(|(name, _)| *name);
        let mut h = blaze_ids::Hasher::new("blaze-cli test: the head rows");
        for (name, digest) in each.iter() {
            h.part(&[name.kind as u8]).part(&name.subject.0).part(&digest.0);
        }
        (h.finish(), u32::try_from(each.len()).unwrap_or(u32::MAX))
    });
    judged.expect("the stored answer opens").expect("an answer opens")
}

/// Expected lines: `heads split 1: R rows, digest D; jobs ...`, `heads split 8: R rows, digest
/// D; jobs ...` (the same R and D), then `heads split rule: S split jobs, B busy, floor F`
/// with S > 1 and B > 1.
#[test]
fn split_head_jobs_answer_as_one_job_and_fill_the_workers() {
    let one = request(&eight_files(41), HeadSplit::Jobs(core::num::NonZeroU32::MIN));
    let eight = request(&eight_files(41), HeadSplit::Jobs(core::num::NonZeroU32::new(8).expect("eight")));
    for (label, said) in [("1", &one), ("8", &eight)] {
        let u = said.checked.seeds.units;
        println!("heads split {label}: {} rows, digest {:?}; jobs {} for {} tasks, {} busy; cells {}, frozen {}, bytes held {}", said.rows.1, said.rows.0, u.split_jobs, u.tasks, u.busy, said.checked.seeds.once.cells, said.checked.seeds.once.frozen, said.checked.seeds.once.bytes_held);
    }
    assert!(one.rows.1 >= 15, "the fixture's head rows were stored: {}", one.rows.1);
    assert_eq!(one.checked.seeds.units.split_jobs, one.checked.seeds.units.crate_jobs, "one split a crate under `Jobs(1)`: {:?}", one.checked.seeds.units);
    assert!(eight.checked.seeds.units.split_jobs > one.checked.seeds.units.split_jobs, "`Jobs(8)` cut the crate: {:?}", eight.checked.seeds.units);
    assert_eq!(one.ids, eight.ids, "the findings are the split's to change: {:?} against {:?}", one.ids, eight.ids);
    assert_eq!(one.checked.status.0, eight.checked.status.0, "the status is the split's to change");
    assert_eq!(one.rows, eight.rows, "EQUAL ROWS: every head row's out, defines and read list, one job against eight");
    let (a, b) = (one.checked.seeds.once, eight.checked.seeds.once);
    // Cells and their bytes (a race's second freeze is counted `twice`, so `frozen` may differ).
    assert_eq!((a.cells, a.bytes_held), (b.cells, b.bytes_held), "EQUAL TABLES: the request's frozen heads, one job against eight: cells {} and {}, bytes {} and {}", a.cells, b.cells, a.bytes_held, b.bytes_held);

    // THE FLOOR under the rule: one crate on eight workers is cut, and more than one job works.
    let rule = request(&eight_files(41), HeadSplit::Rule);
    let u = rule.checked.seeds.units;
    println!("heads split rule: {} split jobs over {} crate jobs, {} busy, floor {}, most {} in one crate", u.split_jobs, u.crate_jobs, u.busy, u.busy_floor, u.most_splits);
    assert!(u.most_splits > 1, "a one-crate unit is cut into more than one job: {u:?}");
    assert!(u.busy > 1, "more than one split job ran a task: {u:?}");
    assert!(u.busy * 100 >= u.busy_floor * 90, "busy jobs within 10% of the floor min(workers, tasks): {u:?}");
    assert_eq!(u.split_files, 0, "the command schedules each file in one split");
    assert_eq!(rule.rows, one.rows, "EQUAL ROWS: the rule's split against one job");
}

#[test]
fn uneven_head_files_stay_whole_and_answer_as_one_job() {
    let jobs = core::num::NonZeroU32::new(4).expect("four");
    let tasks = UNEVEN.iter().sum::<usize>();
    let target = tasks.div_ceil(jobs.get() as usize);
    let largest = *UNEVEN.iter().max().expect("six files");
    let runs: std::vec::Vec<_> = file_splits(&UNEVEN, tasks, jobs.get() as usize).collect();
    assert_eq!(runs, [0..2, 2..42, 42..50], "greedy whole files with the oversized file alone");
    let sizes: std::vec::Vec<_> = runs.iter().map(|run| run.len()).collect();
    assert!(sizes.iter().all(|&size| size <= target + largest));

    let one = request(&uneven_files(), HeadSplit::Jobs(core::num::NonZeroU32::MIN));
    let whole = request(&uneven_files(), HeadSplit::Jobs(jobs));
    let u = whole.checked.seeds.units;
    assert_eq!(u.split_files, 0, "files in multiple dispatched splits");
    assert_eq!(u.most_splits as usize, runs.len(), "actual jobs dispatched for the crate");
    assert_eq!(one.rows.1, tasks as u32, "one stored Signature per function");
    assert_eq!(whole.rows, one.rows, "all Signature rows equal the one-job answer");
    println!("uneven head files: sizes {sizes:?}, bound {}, {} files in more than one split, {} equal rows", target + largest, u.split_files, whole.rows.1);
}

#[test]
fn the_file_split_count_catches_even_task_cuts() {
    let jobs = core::num::NonZeroU32::new(4).expect("four");
    let even = request(&uneven_files(), HeadSplit::Even(jobs));
    let u = even.checked.seeds.units;
    assert_eq!(u.most_splits, jobs.get(), "the control dispatches all four even cuts");
    assert!(u.crate_jobs > 0, "the fixture ran head tasks");
    assert_eq!(u.split_files, u.crate_jobs, "cuts at 12, 25 and 37 all cross the same file in each crate job");
    println!("uneven head files control: 1 file in more than one split per crate job, {} total", u.split_files);
}

#[test]
fn whole_file_cuts_fit_the_job_budget_when_possible() {
    // Target 9. Admitting the next whole file raises the limit to 12 and fits two jobs.
    let sizes: std::vec::Vec<_> = file_splits(&[6, 6, 6], 18, 2).map(|run| run.len()).collect();
    assert_eq!(sizes, [12, 6]);
    // Target 5. Both boundaries of the eight-task file are compulsory.
    let isolated: std::vec::Vec<_> = file_splits(&[1, 8, 1], 10, 2).map(|run| run.len()).collect();
    assert_eq!(isolated, [1, 8, 1]);
    println!("two requested jobs: {sizes:?}; isolated oversized file: {isolated:?}");
}

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

/// Some split jobs, each result its split's mark, finishing in the order the tape draws; the
/// oracle: the merge gives each in split order.
fn merge(tape: &Tape, plant: MergePlant, heap: &'static blaze_own::Heap) -> Result<(), Broke> {
    let alloc = heap.shared();
    let splits = 2 + usize::try_from(tape.below(14, "split jobs, past two")).unwrap_or(0);
    let mut arrived: blaze_engine::SVec<'static, (u32, u64)> = blaze_engine::SVec::with_capacity_in(splits, alloc);
    for at in 0..u32::try_from(splits).unwrap_or(u32::MAX) {
        arrived.push((at, u64::from(at) * 7 + 3));
    }
    tape.shuffle(&mut arrived, "the order the split jobs finished in");
    let merged = match in_split_order(arrived, splits, plant, alloc) {
        Ok(merged) => merged,
        Err(error) => return Err(Broke::Harness(std::string::String::from(if matches!(error, crate::engine_heads::MergeError::Missing { .. }) { "a split's result was lost" } else { "a split's result came twice or past the splits" }))),
    };
    if merged.len() != splits {
        return Err(Broke::Oracle(std::string::String::from("the merge holds other than one result a split")));
    }
    for (at, one) in (0u64..).zip(merged.iter()) {
        if *one != at * 7 + 3 {
            return Err(Broke::Oracle(std::string::String::from("a split's result out of split order")));
        }
    }
    Ok(())
}

/// Every completion order merges in split order.
#[test]
fn split_results_merge_in_split_order_whatever_finished_first() {
    let heap = heap();
    sim_check(Config::cases(500), |tape| merge(tape, MergePlant::Rule, heap));
}

/// CONTROL (rule 23): the results kept in arrival order are caught by the same oracle.
#[test]
fn the_merge_oracle_catches_arrival_order() {
    let heap = heap();
    control(Config::cases(500), MergePlant::Rule, MergePlant::Arrival, "a split's result out of split order", |tape, plant| merge(tape, plant, heap));
}

#[test]
fn large_single_file_splits_across_workers_for_128_cores() {
    let runs: std::vec::Vec<_> = file_splits(&[256], 256, 16).collect();
    assert_eq!(runs.len(), 8, "a 256-task mega-file splits into 8 balanced chunks of 32: {runs:?}");
    assert_eq!(runs.iter().map(|r| r.len()).sum::<usize>(), 256);
    assert_eq!(runs[0], 0..32);
    assert_eq!(runs[7], 224..256);

    let wide: std::vec::Vec<_> = file_splits(&[4096], 4096, 512).collect();
    assert_eq!(wide.len(), 128, "a 4096-task mega-file splits across 128 cores: {}", wide.len());
    assert_eq!(wide.iter().map(|r| r.len()).sum::<usize>(), 4096);

    // Single-job request keeps the file whole.
    let single: std::vec::Vec<_> = file_splits(&[256], 256, 1).collect();
    assert_eq!(single, [0..256]);
}
