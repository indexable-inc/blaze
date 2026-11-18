//! WHAT A BODY LEAVES IN THE REQUEST'S ARENA (lane BD3, 2026-11-18, NEVER COMPILED when
//! written). On tempo the Bodies stage was stopped by memory: what the request's arena kept a
//! body was its row's read list at the capacity of the list BEFORE widening (engine_bodies.rs
//! `widen` made it with the recorded count, and the dedup kept the block), and a dependency's
//! bucket reads one cell a bucket probed. Now the check works in the body's scratch (front2
//! tables/task.rs `check_body_split`, two allocators), the row is copied out at its length
//! (`row_of`), and a dependency's buckets are its tables' `WHOLE` (engine_scope_reads.rs, "A
//! DEPENDENCY'S BUCKETS"). The profile's `bodies memory:` line (engine_bodies.rs `BodyMemLive`,
//! written by the status printer) says the row a body, its floor, and the cells by kind.
//!
//! THE FIXTURE: a toolchain whose `std` (a dependency: its crate defines no cell table) holds
//! [`TYPES`] types, each with an inherent method, and a member of [`BODIES`] bodies, each calling
//! [`PER`] of those methods (a bucket of `std` probed per type) and the member's `base`.
//!
//! The Rust rule the oracle's edit rests on (the Reference, "Function body", "Type coercions"):
//! a body's tail is coerced to its return type, and no coercion turns `u16` into `u8`, so
//! `base` made to return `u16` is one E0308 in every body whose tail is `base()`.

use super::*;
use super::engine_check_tests::{engine_opts, opts, profile};

/// The member's bodies, `std`'s types, and how many of them each body calls.
const BODIES: usize = 300;
const TYPES: usize = 64;
const PER: usize = 40;

/// The most a body's row may hold in the request's arena on this fixture. Its floor: `Done`
/// (about 180 bytes) and its cells at their length (about 81 bytes each): `std`'s and core's
/// records whole, `std`'s tables whole, the member's own names and head cells of `base` and the
/// body, the member's bucket cells, the units before its checker: about 15 cells, 1.4 KB.
/// WRITTEN WITHOUT A RUN: pin to the first run's number rounded up, then tighten toward the
/// floor the line prints. With the plant of the old row (before widening, about 170 cells a
/// body here) it is about 14 KB; with the coarse step off, [`PER`] bucket cells more, about 4.6 KB.
const ROW_MOST: u64 = 2048;

/// A `std` of [`TYPES`] types `T<k>`, each with `get(&self) -> u8` and a maker `t<k>()`, and the
/// prelude modules the fixture's std has (blaze-world `fixture::STD`).
fn wide_std(into: &mut blaze_own::String<&bstd::mem::Frame>) {
    use core::fmt::Write as _;
    into.push_str("#![no_std]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }\n");
    for k in 0..TYPES {
        writeln!(into, "pub struct T{k}(pub u8);\nimpl T{k} {{\n    pub fn get(&self) -> u8 {{\n        self.0\n    }}\n}}\npub fn t{k}() -> T{k} {{\n    T{k}(1)\n}}").expect("a string takes text");
    }
}

/// The member: `base` and [`BODIES`] bodies, the `i`th calling `get` on [`PER`] of `std`'s types
/// from `i` on, its tail `base()`; `base_returns` is `base`'s return type.
fn member(io: &Io, base_returns: &str) {
    use core::fmt::Write as _;
    // The text in a frame of the test's own (no global heap in a line of this lane).
    let frame = bstd::mem::Frame::with_reserve(4 << 20, bstd::mem::Tag::Other).expect("the fixture's frame");
    let mut lib = blaze_own::String::new_in(&frame);
    writeln!(lib, "pub fn base() -> {base_returns} {{\n    1\n}}").expect("a string takes text");
    for i in 0..BODIES {
        writeln!(lib, "pub fn b{i}() -> u8 {{").expect("a string takes text");
        for j in 0..PER {
            writeln!(lib, "    let _ = std::t{}().get();", (i + j) % TYPES).expect("a string takes text");
        }
        writeln!(lib, "    base()\n}}").expect("a string takes text");
    }
    write(io, "Cargo.toml", MANIFEST);
    write(io, "src/lib.rs", lib.as_str());
}

/// The fixture's shelf with [`wide_std`] as its std, written on the home `/h` beside the plain
/// one (main.rs `shelf`), in `alloc`: its id.
fn wide_shelf(io: &Io, alloc: blaze_own::Alloc<'_>) -> blaze_toolchain::ToolchainId {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the std text's frame");
    let mut std_text = blaze_own::String::new_in(&frame);
    wide_std(&mut std_text);
    let (id, files) = blaze_world::fixture::shelf(graph::TRIPLE, std_text.as_str(), SHELF_CFG, alloc);
    for (name, bytes) in files {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let path = bstd::mem::format_in!(alloc, "h/{}/{name}", toolchain::SHELF);
        let path = Path::new(path.as_str()).expect("a path");
        io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write");
    }
    id
}

/// One request on the wide toolchain: what it listed, printed (its closing line dropped) and
/// counted.
struct Answer {
    ids: std::vec::Vec<(std::string::String, std::string::String)>,
    said: std::string::String,
    checked: engine_check::Checked,
}

impl Answer {
    /// The printed lines but the closing one.
    fn lines(&self) -> &str {
        self.said.trim_end().rsplit_once('\n').map_or("", |(lines, _)| lines)
    }

    /// The error lines printed.
    fn errors(&self) -> usize {
        self.lines().lines().filter(|line| line.contains("error[")).count()
    }
}

fn answer(io: &Io, fresh: bool) -> Answer {
    let before = stdout(io).len();
    let mut o = opts(io, 2, fresh);
    o.toolchain = wide_shelf(io, o.heap.alloc());
    let (ids, checked) = engine_opts(io, o);
    let said = stdout(io).split_off(before);
    Answer { ids, said, checked }
}

/// The profile's `bodies memory:` line, read back.
#[derive(Clone, Copy, Debug)]
struct MemLine {
    bodies: u64,
    row: u64,
    row_most: u64,
    floor: u64,
    recorded: u64,
    before: u64,
    after: u64,
    whole: u64,
    buckets: u64,
    scope: u64,
    recorder: u64,
    recorder_floor: u64,
}

fn mem_line(profile: &str) -> MemLine {
    let line = profile.lines().find(|line| line.starts_with("bodies memory: ")).unwrap_or_else(|| panic!("no `bodies memory:` line in the profile:\n{profile}"));
    let number_in = |part: &str, key: &str| -> u64 {
        let at = part.find(key).unwrap_or_else(|| panic!("`{key}` not in {part:?}")) + key.len();
        part[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {part:?}"))
    };
    let number_after = |key: &str| number_in(line, key);
    let scratch_part = line.split_once("in its scratch: ").map_or_else(|| panic!("no scratch part in {line:?}"), |(_, part)| part);
    MemLine {
        bodies: number_after("bodies memory: "),
        row: number_after("request's end "),
        row_most: number_after("bytes a body (most "),
        floor: number_after("; floor "),
        recorded: number_after("recorded "),
        before: number_after("before widening "),
        after: number_after(", after "),
        whole: number_after("whole records "),
        buckets: number_after("bucket cells "),
        scope: number_after("scope crates "),
        recorder: number_in(scratch_part, "recorder "),
        recorder_floor: number_in(scratch_part, "; floor "),
    }
}

/// A BODY LEAVES ITS ROW ALONE, AT ITS LENGTH: on [`BODIES`] bodies each calling [`PER`] of a
/// dependency's methods, the row a body keeps in the request's arena is at most [`ROW_MOST`]
/// bytes and within 10% of its floor (its lists at their lengths), and the widening narrowed it.
/// Prints `BODY ROWS: <R> bytes a body (most <M>; floor <F>, <R/F>x) over <N> bodies; cells
/// recorded <a>, before widening <b>, after <c> (whole records <w>, bucket cells <k>; scope
/// crates <s>); recorder <r> bytes in the scratch, freed as the body ends`.
#[test]
fn a_body_leaves_its_row_alone_at_its_length() {
    let io = Io::simulated(SimWorld::new(47, Limits::USUAL));
    member(&io, "u8");
    let first = answer(&io, true);
    assert_eq!(first.errors(), 0, "the fixture checks clean:\n{}", first.lines());
    let m = mem_line(&profile(&io));
    println!(
        "BODY ROWS: {} bytes a body (most {}; floor {}, {:.2}x) over {} bodies; cells recorded {}, before widening {}, after {} (whole records {}, bucket cells {}; scope crates {}); recorder {} bytes in the scratch, freed as the body ends",
        m.row,
        m.row_most,
        m.floor,
        m.row as f64 / m.floor.max(1) as f64,
        m.bodies,
        m.recorded,
        m.before,
        m.after,
        m.whole,
        m.buckets,
        m.scope,
        m.recorder
    );
    assert!(m.bodies as usize > BODIES, "every body counted (and `base`): {m:?}");
    assert!(m.row >= m.floor, "a row under its floor: the count misses a list: {m:?}");
    assert!(m.row * 10 <= m.floor * 11, "the row is not at its length (over 10% past its floor): {m:?}");
    assert!(m.row <= ROW_MOST, "a body keeps {} bytes in the request's arena, over {ROW_MOST}: {m:?}", m.row);
    assert!(m.after < m.before, "the widening did not narrow the reads: {m:?}");
    assert!(m.recorded > 0 && m.recorder >= m.recorder_floor, "the scratch's recorder is counted: {m:?}");
}

/// THE CAUSES, ONE AT A TIME (rule 23), the answer unchanged by each: (1) the old row planted
/// back (`PLANT_ROW_AT_RECORDED`: its list in the request's arena at the capacity of the list
/// before widening) keeps several times the bytes a body; (2) the coarse step off
/// (`PLANT_FINE_BUCKETS`: a dependency's bucket reads one a bucket) keeps [`PER`]-odd bucket
/// cells a body more, and several times the bytes. Prints `ROW PLANTS: row <R> bytes; old row
/// <O> (<O/R>x); fine buckets <B> (<B/R>x), bucket cells <k> -> <K>`.
#[test]
fn the_old_row_and_the_fine_buckets_each_keep_several_times_more() {
    let io = Io::simulated(SimWorld::new(53, Limits::USUAL));
    member(&io, "u8");
    let now = answer(&io, true);
    let m = mem_line(&profile(&io));
    crate::engine_bodies::PLANT_ROW_AT_RECORDED.with(|plant| plant.set(true));
    let old = answer(&io, true);
    crate::engine_bodies::PLANT_ROW_AT_RECORDED.with(|plant| plant.set(false));
    let o = mem_line(&profile(&io));
    crate::engine_bodies::PLANT_FINE_BUCKETS.with(|plant| plant.set(true));
    let fine = answer(&io, true);
    crate::engine_bodies::PLANT_FINE_BUCKETS.with(|plant| plant.set(false));
    let f = mem_line(&profile(&io));
    println!("ROW PLANTS: row {} bytes; old row {} ({:.1}x); fine buckets {} ({:.1}x), bucket cells {} -> {}", m.row, o.row, o.row as f64 / m.row.max(1) as f64, f.row, f.row as f64 / m.row.max(1) as f64, m.buckets, f.buckets);
    assert_eq!((&old.ids, old.lines()), (&now.ids, now.lines()), "the old row's plant changed the answer");
    assert_eq!((&fine.ids, fine.lines()), (&now.ids, now.lines()), "the fine buckets' plant changed the answer");
    assert!(o.row >= 3 * m.row, "the old row kept {} bytes a body, not several times {}: the row's capacity is not the cause", o.row, m.row);
    assert_eq!(o.after, m.after, "the old row's plant changed which cells a row holds");
    assert!(f.buckets >= m.buckets + (PER as u64) / 2, "the fine buckets kept {} bucket cells a body, not about {PER} more than {}", f.buckets, m.buckets);
    assert!(f.row >= 3 * m.row, "the fine buckets kept {} bytes a body, not several times {}", f.row, m.row);
}

/// THE ORACLE (`Oracle::Equal`'s sense, engine_edit_rows_tests.rs, on this file's toolchain):
/// after a cold request, an edit to the member item every body read (`base` made to return
/// `u16`) still enters every body under the coarse reads: the incremental answer lists the
/// same findings and prints the same lines as a fresh request on the edited files, and has
/// [`BODIES`] E0308s. A request with nothing moved is the certificate's and equals the first.
/// Prints `ORACLE: cold <e0> errors; after the edit <e1> errors, bodies entered <n>; equal a
/// fresh world's: yes`.
#[test]
fn an_edit_a_body_read_still_enters_it_under_the_coarse_reads() {
    let io = Io::simulated(SimWorld::new(59, Limits::USUAL));
    member(&io, "u8");
    let cold = answer(&io, false);
    assert_eq!(cold.errors(), 0, "the fixture checks clean:\n{}", cold.lines());
    let again = answer(&io, false);
    assert_eq!((&again.ids, again.lines()), (&cold.ids, cold.lines()), "nothing moved: the answer is the first's");
    member(&io, "u16");
    let edited = answer(&io, false);
    let entered = edited.checked.stages[blaze_engine::Stage::Bodies.index()].entered;
    let fresh = answer(&io, true);
    let equal = edited.ids == fresh.ids && edited.lines() == fresh.lines() && edited.checked.status.0 == fresh.checked.status.0;
    println!("ORACLE: cold {} errors; after the edit {} errors, bodies entered {entered}; equal a fresh world's: {}", cold.errors(), edited.errors(), if equal { "yes" } else { "no" });
    assert!(equal, "the answer after the edit is not a fresh world's (stale under the coarse reads)\nafter the edit:\n{}\nfresh:\n{}", edited.lines(), fresh.lines());
    assert_eq!(edited.errors(), BODIES, "every body whose tail is `base()` entered and found its E0308:\n{}", edited.lines());
    assert!(entered as usize >= BODIES, "the edit entered {entered} bodies, not every one that read `base`");
}
