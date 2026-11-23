//! WHAT A BODY LEAVES IN THE REQUEST'S ARENA (lane BD3, 2026-11-18, NEVER COMPILED when
//! written). On tempo the Bodies stage was stopped by memory: what the request's arena kept a
//! body was its row's read list at the capacity of the list BEFORE widening (engine_bodies.rs
//! `widen` made it with the recorded count, and the dedup kept the block), and a dependency's
//! bucket reads one cell a bucket probed. Now the check records distinct keys in the body's
//! scratch. The row is copied out at its length (`row_of`), and all settled dependencies
//! share the graph's cell. The profile's `bodies memory:` line (engine_bodies.rs `BodyMemLive`,
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
const PER: usize = 60;

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
    ids_made: u64,
    offers: u64,
    memo_hits: u64,
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
        ids_made: number_after("ids made "),
        offers: number_after(" against "),
        memo_hits: number_after("memo hits "),
    }
}

/// Each reported giant must be a distinct body from the wide member, with the same peak
/// recorded count as the aggregate and a row whose per-kind counts account for all its ids.
fn check_largest_bodies(report: &str) {
    let line = report.lines().find(|line| line.starts_with("bodies memory: ")).expect("body memory line");
    let (aggregate, ranked) = line.split_once(" · #1 ").expect("the largest body");
    let (_, recorded) = aggregate.split_once("read cells a body: recorded ").expect("recorded reads");
    let (_, maximum) = recorded.split_once('/').expect("mean and most");
    let most: u64 = maximum.split(',').next().unwrap().parse().expect("maximum recorded reads");
    let mut previous = most;
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = ranked.split(" · #");
    for rank in 1..=3 {
        let entry = entries.next().expect("three bodies named");
        let body = if rank == 1 { entry } else { entry.strip_prefix(&format!("{rank} ")).expect("rank in order") };
        let (label, counts) = body.strip_prefix("`src/lib.rs` item `").expect("member's file").split_once("`: ").expect("its item");
        let index: usize = label.strip_prefix('b').expect("a caller of the methods").parse().expect("body index");
        assert!(index < BODIES && seen.insert(index), "unexpected or duplicate giant: {body}");
        let (n, counts) = counts.split_once(" reads recorded, ").expect("recorded count");
        let recorded: u64 = n.parse().expect("reads");
        assert!(recorded > 0 && recorded <= previous, "{body}");
        if rank == 1 {
            assert_eq!(recorded, most, "top body has the actual maximum read count");
        }
        previous = recorded;
        let (n, counts) = counts.split_once(" distinct ids, ").expect("distinct count");
        let distinct: u64 = n.parse().expect("ids");
        let (n, rest) = counts.split_once(" scratch bytes").expect("scratch use");
        assert!(distinct > 0 && n.parse::<u64>().expect("scratch") > 0, "{body}");
        if rank == 1 {
            let (keys, row) = rest.split_once("; row whole records ").expect("top body's recorded and row splits");
            for kind in ["recorded names ", "heads ", "impl buckets "] {
                assert!(keys.contains(kind), "{keys}");
            }
            let row = row.trim_end_matches(')');
            let counts: std::vec::Vec<u64> = row.split(", ").map(|s| s.split_whitespace().last().expect("kind count").parse().expect("count")).collect();
            assert_eq!(counts.len(), 5, "whole, names, heads, buckets, other");
            assert_eq!(counts.iter().sum::<u64>(), distinct, "{body}");
        }
    }
    assert!(entries.next().is_none(), "the report retains three bodies");
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
    let report = profile(&io);
    check_largest_bodies(&report);
    let m = mem_line(&report);
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
    assert!(m.ids_made > 0 && m.memo_hits > 0 && m.ids_made < m.offers, "the repeated calls reuse IDs: {m:?}");
    println!("READ IDS: {} made for {} recorded reads, {} memo hits", m.ids_made, m.offers, m.memo_hits);
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

fn dependency_answer(io: &Io, fresh: bool) -> Answer {
    let before = stdout(io).len();
    let mut options = opts(io, 2, fresh);
    options.place.cargo_home = Some("/c");
    options.scope_reads = crate::engine_scope_reads::ScopeReadsAs::Cells;
    let (ids, checked) = engine_opts(io, options);
    Answer { ids, said: stdout(io).split_off(before), checked }
}

const CALLER: &str = "pub fn caller() -> u8 {\n    dep::value()\n}\n";
const BYTE: &str = "pub fn value() -> u8 {\n    1\n}\n";
const WIDE: &str = "pub fn value() -> u16 {\n    1\n}\n";
const DEP_MANIFEST: &str = "[package]\nname = \"dep\"\nversion = \"1.0.0\"\nedition = \"2024\"\n";
const MISMATCH: &str = "error[types]: found `u16`, expected `u8`";

/// Install a real tar.gz in the simulated Cargo cache and put its SHA-256 in the lockfile.
/// Replacing the archive keeps the package name, version, and caller's text identical.
fn registry(io: &Io, source: &str) -> blaze_drive::Name {
    let (name, archive) = locked(io, source);
    cache_archive(io, archive.as_slice());
    name
}

/// `dep`'s archive of `source` and its lock entry written (its SHA-256 the pin), the archive NOT
/// in cargo's cache: the `Fetch` name and the archive's bytes.
fn locked(io: &Io, source: &str) -> (blaze_drive::Name, blaze_own::Vec<u8, blaze_own::Alloc<'static>>) {
    use blaze_tar::{Make, Writer};
    let alloc = blaze_own::testing::heap().alloc();
    let mut tar = blaze_own::Vec::new_in(alloc);
    let mut writer = Writer::new();
    writer.entry("dep-1.0.0/Cargo.toml", Make::File { exec: false }, DEP_MANIFEST.as_bytes(), &mut tar).expect("manifest");
    writer.entry("dep-1.0.0/src/lib.rs", Make::File { exec: false }, source.as_bytes(), &mut tar).expect("library");
    writer.finish(&mut tar).expect("tar trailer");
    let mut archive = blaze_own::Vec::new_in(alloc);
    blaze_deflate::gzip(&tar, blaze_deflate::Level::Fast, alloc, &mut archive).expect("a .crate archive");
    let sha256 = blaze_sha256::digest(&archive);
    let checksum = crate::fetch::key_hex(&blaze_ids::ContentId(sha256));
    let checksum = core::str::from_utf8(&checksum).expect("hex digits");
    write(io, "Cargo.lock", &format!("version = 4\n\n[[package]]\nname = \"wide\"\nversion = \"0.1.0\"\ndependencies = [\n \"dep\",\n]\n\n[[package]]\nname = \"dep\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n"));
    let source_id = blaze_fetch::SourceId::Crate { name: "dep", version: "1.0.0", sha256 };
    (blaze_drive::Name::new(blaze_drive::Kind::Fetch, source_id.key()), archive)
}

/// `archive` put where cargo keeps it (`<cargo home>/registry/cache/<crates.io folder>/`), the
/// cargo home `/c` ([`dependency_answer`]).
fn cache_archive(io: &Io, archive: &[u8]) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("simulated filesystem");
    let path = Path::new("c/registry/cache/index.crates.io-1949cf8c6b5b557f/dep-1.0.0.crate").expect("cache path");
    io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(archive), Durable::No).expect("publish archive");
}

fn with_dependency_rows<R>(io: &Io, inspect: impl FnOnce(&blaze_engine::pages::Kept<'_>) -> R) -> R {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("simulated filesystem");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("checkout path")).expect("checkout");
    let mut phases = Phases::new(io.need_clock().expect("clock"));
    let err = io.need_err().expect("err");
    let fs = io.need_fs().expect("files");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    crate::engine_why::with_kept(&cx, &fs, &host, &mut rng, &root, &opts(io, 2, false), &mut phases, &err, inspect).expect("open answer").expect("persisted answer")
}

/// Check the fetch, the graph it fed, and the proof the caller actually persisted: every cell of
/// the graph the caller read (its whole record's, or since D187 the cell of each settled crate
/// it reads, engine_units.rs `crate_cell`) is one the graph's row defines at the digest read.
/// Return those reads, so the edit must move one of them.
fn check_archive_row(io: &Io, fetch: blaze_drive::Name) -> std::vec::Vec<(blaze_engine::CellId, blaze_ids::ContentId)> {
    use blaze_drive::Kind;
    use crate::engine_tasks::{CELLS, WHOLE};
    let graph = blaze_drive::Name::unit_graph();
    with_dependency_rows(io, |kept| {
        let rows = &kept.answer.rows;
        let fetched = rows.get(&fetch).expect("a Fetch row for the locked checksum");
        assert_eq!(fetched.out, fetch.subject, "Fetch must hold the pinned archive");
        let published = rows.get(&graph).expect("the request's UnitGraph row");
        assert!(!published.failed, "UnitGraph failed after Fetch held {fetch:?}");
        assert!(published.defines.contains(&(WHOLE, published.out)), "UnitGraph must publish its whole record");
        let mut bodies = 0;
        let mut graph_reads = std::vec::Vec::new();
        for (_, row) in rows.iter().filter(|(name, _)| name.kind == Kind::BodyCheck) {
            bodies += 1;
            assert_eq!(row.file, blaze_drive::Name::file(b"src/lib.rs"));
            graph_reads.extend(row.reads.iter().copied().filter(|(id, _)| id.of == graph));
            assert!(!graph_reads.is_empty(), "the caller reads the graph: {:?}", row.reads);
            for (id, saw) in &graph_reads {
                assert!(published.defines.contains(&(id.cell, *saw)), "the caller read {id:?} at a digest the graph does not define: {:?}", published.defines);
            }
            assert!(row.reads.windows(2).all(|w| w[0].0 < w[1].0));
            for (id, _) in &row.reads {
                match id.of.kind {
                    Kind::CrateScope => assert!(rows[&id.of].defines.iter().any(|(cell, _)| *cell == CELLS), "redundant dependency names read {id:?}"),
                    Kind::ImplBucket => assert_ne!(id.cell, WHOLE, "redundant dependency tables read {id:?}"),
                    _ => {}
                }
            }
        }
        assert_eq!(bodies, 2, "one workspace caller, in its 2 units' rows (D191: the library's and its test build's)");
        graph_reads
    })
}

/// The read of `before` (the caller's graph reads before an edit) the edit moved: one not read
/// again at the same digest after it.
fn moved_graph_read(before: &[(blaze_engine::CellId, blaze_ids::ContentId)], after: &[(blaze_engine::CellId, blaze_ids::ContentId)]) -> blaze_engine::CellId {
    before.iter().find(|read| !after.contains(read)).map(|(id, _)| *id).expect("the edit must move a graph cell the caller read")
}

fn assert_body_read_moved(answer: &Answer, id: blaze_engine::CellId) {
    let because = &answer.checked.because;
    assert!(because.iter().any(|(group, count)| {
        group.stage == blaze_engine::Stage::Bodies && group.cause == "read moved" && group.definer == Some(id.of)
            && group.cell == Some(crate::engine_scope_reads::CellKind::of(&id)) && *count > 0
    }), "expected Bodies to enter on {id:?}, got {because:?}");
}

/// The caller's tail can return the original u8. Returning the new u16 requires an explicit
/// conversion, so its one error must appear on the incremental request too.
#[test]
fn a_changed_archive_checksum_enters_the_body_through_the_graph() {
    let io = Io::simulated(SimWorld::new(61, Limits::USUAL));
    write(&io, "Cargo.toml", &format!("{MANIFEST}\n[dependencies]\ndep = \"1\"\n"));
    write(&io, "src/lib.rs", CALLER);
    let original = registry(&io, BYTE);
    let cold = dependency_answer(&io, false);
    assert_eq!(cold.errors(), 0, "{}", cold.lines());
    let before = check_archive_row(&io, original);
    let replacement = registry(&io, WIDE);
    assert_ne!(original, replacement, "the edit changes the archive checksum");
    let edited = dependency_answer(&io, false);
    let after = check_archive_row(&io, replacement);
    assert_body_read_moved(&edited, moved_graph_read(&before, &after));
    let fresh = dependency_answer(&io, true);
    assert_eq!((&edited.ids, edited.lines(), edited.checked.status.0), (&fresh.ids, fresh.lines(), fresh.checked.status.0));
    assert_eq!(edited.errors(), 1, "{}", edited.lines());
    assert!(edited.lines().contains(MISMATCH), "{}", edited.lines());
    println!("ARCHIVE: checksum changed, graph entered caller, 1 {MISMATCH}");
}

/// `name` 1.0.0's `.crate` archive with `source` as its lib.rs, and its SHA-256.
fn crate_archive(name: &str, source: &str) -> ([u8; 32], blaze_own::Vec<u8, blaze_own::Alloc<'static>>) {
    use blaze_tar::{Make, Writer};
    let alloc = blaze_own::testing::heap().alloc();
    let mut tar = blaze_own::Vec::new_in(alloc);
    let mut writer = Writer::new();
    let manifest = format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2024\"\n");
    writer.entry(&format!("{name}-1.0.0/Cargo.toml"), Make::File { exec: false }, manifest.as_bytes(), &mut tar).expect("manifest");
    writer.entry(&format!("{name}-1.0.0/src/lib.rs"), Make::File { exec: false }, source.as_bytes(), &mut tar).expect("library");
    writer.finish(&mut tar).expect("tar trailer");
    let mut archive = blaze_own::Vec::new_in(alloc);
    blaze_deflate::gzip(&tar, blaze_deflate::Level::Fast, alloc, &mut archive).expect("a .crate archive");
    (blaze_sha256::digest(&archive), archive)
}

/// Two registry dependencies, `dep` of `dep_source` and `pin` of [`BYTE`], both in cargo's cache
/// and both pinned by the lock for a 2-member workspace (`a` -> `dep`, `b` -> `pin`).
fn two_registry(io: &Io, dep_source: &str) {
    let (dep_sum, dep) = crate_archive("dep", dep_source);
    let (pin_sum, pin) = crate_archive("pin", BYTE);
    let hex = |sum: [u8; 32]| std::string::String::from(core::str::from_utf8(&crate::fetch::key_hex(&blaze_ids::ContentId(sum))).expect("hex digits"));
    let entry = |name: &str, sum: [u8; 32]| format!("[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{}\"\n", hex(sum));
    write(
        io,
        "Cargo.lock",
        &format!(
            "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\n \"dep\",\n]\n\n[[package]]\nname = \"b\"\nversion = \"0.1.0\"\ndependencies = [\n \"pin\",\n]\n\n{}\n{}",
            entry("dep", dep_sum),
            entry("pin", pin_sum)
        ),
    );
    let cx = io.cx(BUDGET, TASK_FRAME).expect("simulated filesystem");
    let fs = io.need_fs().expect("files");
    for (name, archive) in [("dep", &dep), ("pin", &pin)] {
        let path = format!("c/registry/cache/index.crates.io-1949cf8c6b5b557f/{name}-1.0.0.crate");
        fs.root().publish(&cx, &Path::new(&path).expect("cache path"), Content::bytes(archive.as_slice()), Durable::No).expect("publish archive");
    }
}

/// The bodies the request entered, by the Bodies groups of its "entered because" lines.
fn bodies_entered(answer: &Answer) -> u32 {
    answer.checked.because.iter().filter(|(group, _)| group.stage == blaze_engine::Stage::Bodies).map(|(_, count)| *count).sum()
}

/// A DEPENDENCY BUMPED ENTERS ONLY THE BODIES THAT READ IT (D187: the graph's crate cells,
/// engine_units.rs `crate_cell`). Two workspace members (`a` depending on `dep`, `b` depending on
/// `pin`) and two bodies, `a::caller` reading `dep` and `b::pinned` reading `pin`. `dep`'s archive
/// is replaced (its checksum, so its crate key and `a`'s key, move); `pin`, `b`, and both callers'
/// text stand. Only `a::caller` enters, on a graph crate cell, and its answer equals a fresh
/// check's. CONTROL (the cause removed, rule 23): with the graph's whole cell read for every
/// settled crate (`PLANT_WHOLE_GRAPH`, the grain before D187), the same bump enters both bodies.
#[test]
fn a_dependency_bumped_enters_only_the_bodies_that_read_it() {
    for plant in [false, true] {
        let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
        write(&io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ndep = \"1\"\n");
        write(&io, "a/src/lib.rs", "pub fn caller() -> u8 {\n    dep::value()\n}\n");
        write(&io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\npin = \"1\"\n");
        write(&io, "b/src/lib.rs", "pub fn pinned() -> u8 {\n    pin::value()\n}\n");
        crate::engine_scope_reads::PLANT_WHOLE_GRAPH.with(|p| p.set(plant));
        two_registry(&io, BYTE);
        let cold = dependency_answer(&io, false);
        assert_eq!(cold.errors(), 0, "{}", cold.lines());
        two_registry(&io, WIDE);
        let edited = dependency_answer(&io, false);
        let fresh = dependency_answer(&io, true);
        crate::engine_scope_reads::PLANT_WHOLE_GRAPH.with(|p| p.set(false));
        let entered = bodies_entered(&edited);
        println!("BUMP (plant {plant}): {entered} bodies entered; because {:?}", edited.checked.because);
        assert_eq!((&edited.ids, edited.lines(), edited.checked.status.0), (&fresh.ids, fresh.lines(), fresh.checked.status.0), "plant {plant}: the edit's answer is a fresh check's");
        assert_eq!(edited.errors(), 1, "{}", edited.lines());
        // D191: each body is checked in its 2 units' rows (the library's and its test build's),
        // and a row is entered by its own reads: a body entered is 2 rows entered.
        if plant {
            assert_eq!(entered, 4, "CONTROL: the whole graph's cell read by both bodies enters both, 2 rows each (D191)");
            continue;
        }
        assert_eq!(entered, 2, "only `a::caller` reads `dep`, in its 2 units' rows (D191): {:?}", edited.checked.because);
        assert!(edited.checked.because.iter().any(|(group, count)| group.stage == blaze_engine::Stage::Bodies && group.cell == Some(crate::engine_scope_reads::CellKind::GraphCrate) && *count == 2), "entered on `dep`'s graph crate cell, both of caller's rows (D191): {:?}", edited.checked.because);
    }
}

/// A FETCH THAT FAILED OUTSIDE THE CHECKOUT IS OWED, NEVER REPLAYED (turso, 2026-11-20: after a
/// command whose 152 fetches failed, their sources were put in cargo's cache, and the next
/// command replayed the 152 failures from the stored answer's certificate). The first command
/// finds `dep` in no place on the machine and has no network (fetch.rs `NETWORK`): its fetch
/// fails, owed (engine_units.rs `owes`). The archive is then put in cargo's cache, and nothing in
/// the checkout moves. The second command must not stand on the certificate: it runs the fetch
/// again, adopts the archive by the lock's pin and checks the caller with no failure, and its
/// plan line counts one fetch owed again. A third stands on its certificate (nothing owed).
/// CONTROL (the cause removed, rule 23): with nothing owed (`OWES_NOTHING`), the second command
/// stands on the first's certificate and prints the first's failure again: the bug.
#[test]
fn a_fetch_failed_for_want_of_its_source_is_fetched_by_the_next_command() {
    for plant in [false, true] {
        let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
        write(&io, "Cargo.toml", &format!("{MANIFEST}\n[dependencies]\ndep = \"1\"\n"));
        write(&io, "src/lib.rs", CALLER);
        let (fetch, archive) = locked(&io, BYTE);
        crate::engine_units::OWES_NOTHING.with(|p| p.set(plant));
        crate::fetch::NETWORK.with(|n| n.set(false));
        let first = dependency_answer(&io, false);
        crate::fetch::NETWORK.with(|n| n.set(true));
        // fetch.rs: with `fetch` the Hole's words name the unit and the client's refusal; without
        // it, the Done that cannot fetch names the unit and the places tried.
        #[cfg(feature = "fetch")]
        assert!(first.lines().contains("no network in this test"), "the first command's fetch fails, naming why:\n{}", first.lines());
        assert!(first.lines().contains("crate dep 1.0.0"), "the failure names its source:\n{}", first.lines());
        assert_eq!(first.checked.plan.owed_again, 0, "nothing was owed before the first command");
        cache_archive(&io, archive.as_slice());
        // The network stays off: the second command can only take the source from cargo's cache.
        crate::fetch::NETWORK.with(|n| n.set(false));
        let second = dependency_answer(&io, false);
        crate::fetch::NETWORK.with(|n| n.set(true));
        crate::engine_units::OWES_NOTHING.with(|p| p.set(false));
        println!(
            "OWED (plant {plant}): second command certified {}, owed again {}, still owed {}, said:\n{}",
            second.checked.certified, second.checked.plan.owed_again, second.checked.plan.owed_still, second.lines()
        );
        if plant {
            assert!(second.checked.certified && second.lines().contains("crate dep 1.0.0"), "CONTROL: with nothing owed the failure is replayed:\n{}", second.lines());
            continue;
        }
        assert!(!second.checked.certified, "a failed fetch is owed: no certificate stands on it");
        assert!(!second.lines().contains("crate dep 1.0.0"), "the source in cargo's cache is fetched:\n{}", second.lines());
        assert_eq!((second.checked.plan.owed_again, second.checked.plan.owed_still), (1, 0), "one fetch owed again, none still owed");
        assert_eq!(second.errors(), 0, "the caller checks against the fetched `dep`:\n{}", second.lines());
        check_archive_row(&io, fetch);
        let third = dependency_answer(&io, false);
        assert!(third.checked.certified, "nothing owed, nothing moved: the certificate stands\n{}", third.lines());
    }
}

#[test]
fn a_path_dependency_edit_needs_its_own_cell() {
    for plant in [false, true] {
        let io = Io::simulated(SimWorld::new(67, Limits::USUAL));
        write(&io, "Cargo.toml", "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"2\"\n");
        write(&io, "dep/Cargo.toml", DEP_MANIFEST);
        write(&io, "dep/src/lib.rs", BYTE);
        write(&io, "app/Cargo.toml", &format!("{MANIFEST}\n[dependencies]\ndep = {{ path = \"../dep\" }}\n"));
        write(&io, "app/src/lib.rs", CALLER);
        let cold = crate::engine_bodies::PLANT_ALL_DEPENDENCIES_SETTLED.with(|p| {
            p.set(plant);
            let answer = dependency_answer(&io, false);
            p.set(false);
            answer
        });
        assert_eq!(cold.errors(), 0, "{}", cold.lines());
        write(&io, "dep/src/lib.rs", WIDE);
        let edited = dependency_answer(&io, false);
        if !plant {
            let head = with_dependency_rows(&io, |kept| {
                let rows = &kept.answer.rows;
                let app = blaze_drive::Name::file(b"app/src/lib.rs");
                let (caller, _) = rows.iter().find(|(name, row)| name.kind == blaze_drive::Kind::BodyCheck && row.file == app).expect("app's caller");
                let (request, cause) = kept.causes[caller];
                assert_eq!(request, kept.answer.serial, "the edited request must enter the caller");
                let blaze_engine::Cause::ReadMoved { read, .. } = cause else { panic!("caller entered for {cause:?}") };
                assert_eq!(crate::engine_scope_reads::CellKind::of(&read), crate::engine_scope_reads::CellKind::HeadCell);
                assert_eq!(rows[&read.of].file, blaze_drive::Name::file(b"dep/src/lib.rs"), "the caller read dep::value's head");
                read
            });
            assert_body_read_moved(&edited, head);
        }
        let fresh = dependency_answer(&io, true);
        assert_eq!(fresh.errors(), 1, "{}", fresh.lines());
        assert!(fresh.lines().contains(MISMATCH), "{}", fresh.lines());
        if plant {
            assert_eq!(edited.errors(), 0, "the plant must retain the stale u8 answer: {}", edited.lines());
            assert_ne!(edited.ids, fresh.ids);
        } else {
            assert_eq!((&edited.ids, edited.lines(), edited.checked.status.0), (&fresh.ids, fresh.lines(), fresh.checked.status.0));
        }
        println!("PATH: all dependencies planted settled = {plant}, incremental {}, fresh 1 ({MISMATCH})", edited.errors());
    }
}
