//! A HEAD THAT DOES NOT FREEZE IS FAILED ONCE A REQUEST (engine_heads_once.rs, lane HF1), at the
//! engine's seam. A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE CAUSE (tempo, 2026-11-18: the Heads stage 577 s, 98.9% of it under engine_heads_once.rs
//! `alone`): every head that did not freeze was frozen again over a source of its own.
//!
//! THE SEAM: two members whose heads fail by themselves (`a::Bad`, a field of a type no crate
//! declares), by a need (`a::Holder`, `a::Outer`; `b::Wrap`, a member's failed head), by two
//! needs (`b::Both`), and a fn taking one (`b::take`); and heads that freeze. The answer under
//! the job sources (`Sources::Job`) equals the one of a source a head (`Sources::Head`, the
//! strategy kept as the oracle, rule 78) and a fresh check's; with the job sources no head is
//! frozen alone, and each distinct cell is frozen or failed once (a second only by a race,
//! counted). THE TABLE'S RULE for failures: two equal failures of one cell keep the first and
//! count the second, other words or another need are refused, and a cell is never both.

use super::engine_check_tests::{engine_opts, opts};
use super::*;

use crate::engine_heads_once::{Entry, FailedEntry, HeadsOnce, OnceCounts, OnceError, OncePlant, Put, Sources};
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{Known, KnownHeads, NotTheirs};

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
const A_LIB: &str = "pub struct Bad {\n    pub x: Missing,\n}\n\npub struct Holder {\n    pub b: Bad,\n}\n\npub struct Outer {\n    pub h: Holder,\n}\n\npub struct Fine {\n    pub v: u32,\n}\n\npub fn ok(f: Fine) -> u32 {\n    f.v\n}\n";
const B_LIB: &str = "pub struct Other {\n    pub y: AlsoMissing,\n}\n\npub struct Wrap {\n    pub h: a::Holder,\n}\n\npub struct Both {\n    pub o: a::Outer,\n    pub t: Other,\n}\n\npub fn take(b: Both) -> u32 {\n    0\n}\n\npub fn fine(f: a::Fine) -> u32 {\n    a::ok(f)\n}\n";

fn two_members(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "a/src/lib.rs", A_LIB);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "b/src/lib.rs", B_LIB);
    io
}

/// What one request answered: the findings listed, the printed finding and note lines (sorted),
/// the status and the counts.
struct Said {
    ids: std::vec::Vec<(std::string::String, std::string::String)>,
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

/// Expected line: `heads failed once: job: cells C, frozen F, failed cells X, failures published
/// P, twice T+U, alone 0, lowered L; a source a head: alone A, lowered L'; equal answers`.
#[test]
fn a_failed_head_is_failed_once_and_never_frozen_alone() {
    let head_io = two_members(31);
    let by_head = request(&head_io, false, Sources::Head);
    let io = two_members(31);
    let job = request(&io, false, Sources::Job);
    let fresh = request(&io, true, Sources::Job);
    let (o, h) = (job.once(), by_head.once());
    assert!(o.failed_cells >= 3, "PRECONDITION: the fixture's heads did not fail (`Missing` resolved?): {o:?}");
    assert!(job.same_answer(&by_head), "ORACLE: the job sources' answer is not a source a head's: ids {:?} against {:?}, lines {:?} against {:?}", job.ids, by_head.ids, job.lines, by_head.lines);
    assert!(job.same_answer(&fresh), "the job sources' answer is not a fresh check's: {:?} against {:?}", job.lines, fresh.lines);
    // THE CAUSE GONE: no head frozen alone with the job sources; every head with a source a head.
    assert_eq!(o.alone, 0, "ORACLE: a head was frozen alone under the job sources: {o:?}");
    assert!(h.alone > 0, "PRECONDITION: the source-a-head strategy froze no head alone: {h:?}");
    // THE FLOOR (rule 67): each distinct cell frozen or failed once; a second only by a race.
    assert_eq!(o.frozen + o.failed, o.cells + o.failed_cells + o.twice + o.failed_twice, "ORACLE: a cell was frozen or failed more than once outside a race: {o:?}");
    assert_eq!(o.reads_unequal, 0, "two puts of one cell kept other reads: {o:?}");
    // A failed head's need is lowered once a request, not once a reader of it.
    assert!(o.lowered <= h.lowered, "ORACLE: the job sources lowered more than a source a head: {o:?} against {h:?}");
    println!(
        "heads failed once: job: cells {}, frozen {}, failed cells {}, failures published {}, twice {}+{}, alone {}, lowered {}, reads a head task: mean {}, most {}; a source a head: alone {}, lowered {}, reads a head task: mean {}, most {}; equal answers",
        o.cells,
        o.frozen,
        o.failed_cells,
        o.failed,
        o.twice,
        o.failed_twice,
        o.alone,
        o.lowered,
        o.read_mean(),
        o.read_most,
        h.alone,
        h.lowered,
        h.read_mean(),
        h.read_most
    );
}

// THE TABLE'S RULE FOR FAILURES.

fn cell() -> Want {
    Want::Head(blaze_id::ItemKey(blaze_id::ContentId::of("blaze-cli heads failed once", b"the cell")))
}

fn need() -> Want {
    Want::Head(blaze_id::ItemKey(blaze_id::ContentId::of("blaze-cli heads failed once", b"its need")))
}

fn failed(said: &'static str, needs: Option<Want>) -> FailedEntry {
    let cause = crate::engine_head_notes::Cause::new(crate::engine_head_notes::RootKind::Head, said);
    FailedEntry { want: cell(), cause, needs, reads: &[], heads: &[] }
}

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

/// Expected: passes. Equal failures: one entry, the second counted; other words, another need,
/// or a frozen put of a failed cell: typed refusals; a failed cell is answered by its words.
#[test]
fn two_failures_of_one_cell_leave_one_entry() {
    let table: HeadsOnce = HeadsOnce::new(heap(), OncePlant::Rule, Sources::Job);
    assert_eq!(table.put_failed(failed("its own reason", None)), Ok(Put::Kept));
    assert_eq!(table.put_failed(failed("its own reason", None)), Ok(Put::Twice));
    assert_eq!(table.put_failed(failed("other words", None)), Err(OnceError::UnequalFailure { cell: cell() }));
    assert_eq!(table.put_failed(failed("its own reason", Some(need()))), Err(OnceError::UnequalFailure { cell: cell() }));
    let frozen = Entry { want: cell(), member: true, bytes: b"bytes", out: blaze_ids::ContentId::of("blaze-cli heads failed once", b"bytes"), reads: &[], heads: &[], consts: &[], record: None };
    assert_eq!(table.put(frozen), Err(OnceError::FrozenAndFailed { cell: cell() }));
    let counts = table.counts();
    assert_eq!((counts.failed_cells, counts.failed, counts.failed_twice, counts.cells), (1, 4, 1, 0), "{counts:?}");
    assert_eq!(table.look_failed(cell()).map(|e| e.cause.words), Some("its own reason"));
    assert!(matches!(table.known(cell()), Known::Refused(NotTheirs::Failed { said: "its own reason" })), "a failed cell is not answered by its words");
    // And the other way: a frozen cell is never put failed.
    let other: HeadsOnce = HeadsOnce::new(heap(), OncePlant::Rule, Sources::Job);
    assert_eq!(other.put(frozen), Ok(Put::Kept));
    assert_eq!(other.put_failed(failed("its own reason", None)), Err(OnceError::FrozenAndFailed { cell: cell() }));
}
