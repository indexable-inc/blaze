//! EACH HEAD FROZEN ONCE A REQUEST (engine_heads_once.rs, lane RH6), at the engine's seam and
//! under a race. A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE SEAM: two members, `b`'s heads reading `a`'s struct. Cold, the answer equals a fresh
//! check's and the one a source a head gives (`Sources::Head`, the measure before the table),
//! every cell is frozen once (`frozen` = `cells` + the races' `twice`), and the read lists are
//! printed before and after. Then each edit row is compared with a fresh check: a body-only edit
//! of `a` (EARLY CUTOFF: `a`'s task reruns, `b`'s are not entered), and `a`'s struct gaining a
//! field (`b`'s readers are entered).
//!
//! THE RACE (rule 43, `blaze_sim`): two workers that both miss a cell and freeze it, interleaved
//! every way the tape draws (each a look, then a put on a miss): one entry, the first's bytes, an
//! equal second counted `twice`, other bytes refused. CONTROL: the planted "second write wins"
//! (`OncePlant::SecondWins`) is caught.

use super::engine_check_tests::{engine_opts, entries, opts};
use super::*;

use crate::engine_heads_once::{Entry, HeadsOnce, OnceCounts, OnceError, OncePlant, Put, Sources};
use blaze_engine::Stage;
use blaze_ladder::front2::heads::Want;
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

/// The ids a request listed, by the helpers' own form.
type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
const A_LIB: &str = "pub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
/// A body-only edit: `make`'s body.
const A_LIB_BODY: &str = "pub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    let q = p;\n    q\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
/// `Point` gains a field.
const A_LIB_FIELD: &str = "pub struct Point {\n    pub x: u32,\n    pub y: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
const B_LIB: &str = "pub struct Holder {\n    pub p: a::Point,\n}\n\npub fn wrap(p: a::Point) -> Holder {\n    Holder { p }\n}\n\npub fn first(h: &Holder) -> u32 {\n    h.p.x\n}\n\npub fn again(p: a::Point) -> a::Point {\n    a::make(p)\n}\n";

fn two_members(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "a/src/lib.rs", A_LIB);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "b/src/lib.rs", B_LIB);
    io
}

/// What one request answered: the findings listed, the printed finding and note lines (sorted:
/// their order is not promised), the status and the counts.
struct Said {
    ids: Ids,
    lines: std::vec::Vec<std::string::String>,
    checked: engine_check::Checked,
}

impl Said {
    fn same_answer(&self, other: &Said) -> bool {
        self.ids == other.ids && self.lines == other.lines && self.checked.status.0 == other.checked.status.0
    }

    fn once(&self) -> OnceCounts {
        self.checked.seeds.once
    }

    fn heads(&self) -> u32 {
        self.checked.stages[Stage::Heads.index()].entered
    }
}

/// One request on 2 workers, its head tasks over `sources`.
fn request(io: &Io, fresh: bool, sources: Sources) -> Said {
    let mut o = opts(io, 2, fresh);
    o.head_sources = sources;
    let before = stdout(io).len();
    let (ids, checked) = engine_opts(io, o);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().filter(|l| l.contains("error[") || l.contains("warning[") || l.starts_with("note:")).map(str::to_owned).collect();
    lines.sort_unstable();
    Said { ids, lines, checked }
}

/// The row after an edit: the incremental answer against a fresh check's (rule 21: no row is
/// fast by being stale), printed with its counts.
fn row(io: &Io, name: &str) -> Said {
    let now = request(io, false, Sources::Job);
    let fresh = request(io, true, Sources::Job);
    let o = now.once();
    println!(
        "row {name}: heads {} entered; cells {}, frozen {}, twice {}, found {}; reads a head task: mean {}, most {}; equal a fresh check: {}; {}",
        now.heads(),
        o.cells,
        o.frozen,
        o.twice,
        o.found,
        o.read_mean(),
        o.read_most,
        now.same_answer(&fresh),
        entries(&now.checked)
    );
    assert!(now.same_answer(&fresh), "row {name}: the answer after the edit is not a fresh check's: ids {:?} against {:?}, lines {:?} against {:?}", now.ids, fresh.ids, now.lines, fresh.lines);
    now
}

/// Expected lines: `heads once cold: ...`, `heads once before (a source a head): reads a head
/// task mean M, most X; after (the table): mean M', most X'`, then one `row` line an edit.
#[test]
fn each_head_is_frozen_once_and_its_readers_follow_its_meaning() {
    // Before: a source a head (the measure's baseline), on a world of its own.
    let before_io = two_members(23);
    let before = request(&before_io, false, Sources::Head);
    let io = two_members(23);
    let cold = request(&io, false, Sources::Job);
    let fresh = request(&io, true, Sources::Job);
    assert!(cold.same_answer(&fresh), "cold: the table's answer is not a fresh check's: {:?} against {:?}", cold.lines, fresh.lines);
    assert!(cold.same_answer(&before), "cold: the table's answer is not a source a head's: {:?} against {:?}", cold.lines, before.lines);
    let o = cold.once();
    assert!(o.cells > 0, "cold: the table holds no cell: {o:?}");
    // The floor: each cell frozen once; a race between two jobs that both missed a cell is the
    // only second freeze, and it is counted.
    assert_eq!(o.frozen, o.cells + o.twice, "ORACLE: a cell was frozen more than once outside a race: {o:?}");
    assert_eq!(o.reads_unequal, 0, "two freezes of one cell kept other reads: {o:?}");
    // (`Checked` holds no profile text: the table's profile line is read on the real run.)
    let b = before.once();
    println!(
        "heads once before (a source a head): reads a head task mean {}, most {} (cells), lowered {}, constants evaluated {}; after (the table): mean {}, most {}, lowered {}, constants evaluated {} (distinct {})",
        b.read_mean(),
        b.read_most,
        b.lowered,
        b.evaluated,
        o.read_mean(),
        o.read_most,
        o.lowered,
        o.evaluated,
        o.consts
    );
    assert!(o.read_most <= b.read_most && o.read_mean() <= b.read_mean(), "ORACLE: the table's read lists are longer than a source a head's: {o:?} against {b:?}");
    assert!(o.lowered <= b.lowered, "ORACLE: the table lowered more than a source a head: {o:?} against {b:?}");
    println!("heads once cold: {o:?}; {}", entries(&cold.checked));

    // A body-only edit in `a`: `make`'s head task reruns (its key holds its body's digest) and
    // freezes equal bytes, so nothing reading it is entered (EARLY CUTOFF), `b`'s heads least.
    write(&io, "a/src/lib.rs", A_LIB_BODY);
    let body = row(&io, "a body-only edit in a");
    assert_eq!(body.heads(), 1, "ORACLE (early cutoff): a body-only edit entered head tasks beyond the edited fn's own");

    // `Point` gains a field: its head moves, and `b`'s heads that read it are entered.
    write(&io, "a/src/lib.rs", A_LIB_FIELD);
    let field = row(&io, "a's struct gains a field");
    assert!(field.heads() >= 2, "ORACLE: `Point` gained a field and no head task of `b` that reads it was entered ({} entered)", field.heads());
}

// THE RACE.

/// One cell of the race, and its two freezes' bytes.
fn cell() -> Want {
    Want::Head(blaze_id::ItemKey(blaze_id::ContentId::of("blaze-cli heads once race", b"the cell")))
}

const ONE: &[u8] = b"the first freeze's bytes";
const OTHER: &[u8] = b"another freeze's bytes!!";

fn entry(bytes: &'static [u8]) -> Entry {
    Entry { want: cell(), member: false, bytes, out: blaze_ids::ContentId::of("blaze-cli heads once race", bytes), reads: &[], heads: &[], consts: &[] }
}

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

/// Two workers, each a look then (on a miss) a put of its freeze, interleaved as the tape draws;
/// the second's bytes equal or other. The oracle over the table after both.
fn race(tape: &Tape, plant: OncePlant, heap: &'static blaze_own::Heap) -> Result<(), Broke> {
    let table = HeadsOnce::new(heap, plant, Sources::Job);
    let other = tape.bool("the second freeze made other bytes");
    let bytes = [ONE, if other { OTHER } else { ONE }];
    // Each worker's next step: 0 look, 1 put, 2 done.
    let mut step = [0u8; 2];
    let mut missed = [false; 2];
    let mut puts: std::vec::Vec<(usize, Result<Put, OnceError>)> = std::vec::Vec::new();
    while step != [2, 2] {
        let w = match (step[0] < 2, step[1] < 2) {
            (true, true) => usize::from(tape.bool("the second worker steps now")),
            (true, false) => 0,
            _ => 1,
        };
        match step[w] {
            0 => {
                missed[w] = table.look(cell()).is_none();
                step[w] = if missed[w] { 1 } else { 2 };
            }
            _ => {
                puts.push((w, table.put(entry(bytes[w]))));
                step[w] = 2;
            }
        }
    }
    let counts = table.counts();
    let oracle = |what: std::string::String| Err(Broke::Oracle(what));
    if counts.cells != 1 {
        return oracle(std::format!("the table holds {} entries of one cell", counts.cells));
    }
    let Some((first, _)) = puts.first().copied() else {
        return Err(Broke::Harness(std::string::String::from("no worker put")));
    };
    let held = table.look(cell()).map(|e| e.bytes);
    if held != Some(bytes[first]) {
        return oracle(std::string::String::from("the table holds other bytes than the first freeze's"));
    }
    match puts.as_slice() {
        [(_, Ok(Put::Kept))] if counts.twice == 0 => Ok(()),
        [(_, Ok(Put::Kept)), (_, Ok(Put::Twice))] if !other && counts.twice == 1 => Ok(()),
        [(_, Ok(Put::Kept)), (_, Err(OnceError::Unequal { .. }))] if other && counts.twice == 0 => Ok(()),
        seen => oracle(std::format!("the puts answered {seen:?} with twice {} (other bytes: {other})", counts.twice)),
    }
}

/// Expected: passes over every interleaving the tape draws.
#[test]
fn two_freezes_of_one_cell_leave_one_entry() {
    let heap = heap();
    sim_check(Config::cases(500), |tape| race(tape, OncePlant::Rule, heap));
}

/// CONTROL (rule 23): a second write that wins is caught by the same oracle.
#[test]
fn the_race_oracle_catches_a_second_write_that_wins() {
    let heap = heap();
    control(Config::cases(2000), OncePlant::Rule, OncePlant::SecondWins, "other bytes than the first freeze's", |tape, plant| race(tape, plant, heap));
}
