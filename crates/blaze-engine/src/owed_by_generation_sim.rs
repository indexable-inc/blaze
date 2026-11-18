//! A cell owed by generation alone (lane RD10; blaze-cli's history test, requests 17 to 19, in
//! four requests and two tasks): `MAKER` defines a cell from its text, `READER` reads it.
//!
//! 1. Text 1, everything run: nothing owed.
//! 2. Text 2, the reader not run: the cell owed in both forms.
//! 3. Text 1 again, the reader not run: the cell ends on the digest the reader's row saw. By
//!    digest (`Answer`) the reader is today's for it and the cell settles; by generation
//!    (`GenLast`, as blaze-cli's rows form) the reader's row is older than the cell's move and it
//!    stays owed. The designed difference: the generation form keeps no digest a row saw.
//! 4. Nothing edited, everything run: the generation form enters the reader again (from the owed
//!    cell), its row comes out identical and is stamped with this request's serial, and the cell
//!    settles. The bound: a cell owed by generation alone settles in the first request that
//!    runs its readers.
//!
//! The oracle after each request: the generation form owes every cell the digest form owes;
//! each cell it owes beyond is explained (every reader saw today's digest, some reader's row is
//! older than the move); after a request that ran every task there is none. The control plants
//! `Plant::StaleKeptOnTake` in the generation form's engine: the cell never settles, and the
//! bound names it.

use std::format;
use std::println;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_sim::Broke;

use crate::engine::{Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Finished, Phase, Plant, Planned, SVec, Scope, Stage, Task, Tools, View, drive};
use crate::last::LastAnswer;
use crate::last_gen_sim::{Bug, GenLast};
use crate::overlay::Fold;
use crate::sim::{Pools, alloc, oracle};

const TOOL: Tool = Tool(ContentId([81; 32]));
const FILE: Name = Name::new(Kind::FileItems, ContentId([82; 32]));
const MAKER: Name = Name::new(Kind::Signature, ContentId([83; 32]));
const READER: Name = Name::new(Kind::BodyCheck, ContentId([84; 32]));
const HELD: Cell = Cell([6; 16]);
const THE_CELL: CellId = CellId { of: MAKER, cell: HELD };

/// Every task, or every task but the reader.
struct Runs {
    reader: bool,
}

impl Scope for Runs {
    fn runs(&self, task: &Planned) -> bool {
        self.reader || task.name != READER
    }
}

fn plan(text: u8) -> [Planned; 2] {
    [
        Planned { name: MAKER, phase: Phase::new(Stage::Heads, 0), source: ContentId([text; 32]), file: FILE },
        Planned { name: READER, phase: Phase::new(Stage::Bodies, 0), source: ContentId([85; 32]), file: FILE },
    ]
}

/// The maker defines its text; the reader reads the cell and makes a digest of what it saw.
fn work<L: LastAnswer>(view: View<'_, 'static, L>, task: &Task, buffer: &mut Buffer<'static>) {
    let a = alloc();
    let name = task.planned.name;
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(a);
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(a);
    let out = match name == READER {
        true => {
            let seen = view.cell(&THE_CELL);
            reads.push((THE_CELL, seen));
            key::digest("owed_by_generation_sim reader", &seen.0)
        }
        false => {
            defines.push((HELD, task.planned.source));
            task.planned.source
        }
    };
    buffer.done.push(Done { name, key: task.key, out, defines, reads, findings: 0, failed: false });
}

fn one<L: Fold<'static> + Sync>(last: L, text: u8, reader: bool, plant: Plant) -> Result<Finished<'static, L::Out>, Broke> {
    let plan = plan(text);
    let files: [(Name, &[Planned]); 1] = [(FILE, &plan[..])];
    let run = || -> Result<Finished<'static, L::Out>, EngineError> {
        let mut engine = Engine::new_in(Tools([TOOL; key::KINDS]), last, &Edit { files: &files }, Runs { reader }, plant, alloc())?;
        let mut buffers: StdVec<Buffer<'static>> = StdVec::from([Buffer::new_in(alloc())]);
        drive(&mut engine, &mut Pools::One, &mut buffers, &|_, view, task, buffer| work(view, task, buffer))?;
        engine.finish()
    };
    run().map_err(|error| Broke::Harness(format!("a request failed: {error}")))
}

fn owed<L: LastAnswer>(form: &L) -> Result<StdVec<CellId>, Broke> {
    let cells = form.owed_cells(Alloc::from(alloc())).map_err(|error| Broke::Harness(format!("the owed cells: {error}")))?;
    let mut out: StdVec<CellId> = cells.iter().filter_map(|(cell, _)| form.cell_id(*cell)).collect();
    out.sort_unstable();
    Ok(out)
}

/// Whether each reader of `id` saw today's digest of it, as the form judges.
fn saws<L: LastAnswer>(form: &L, id: &CellId) -> Result<StdVec<bool>, Broke> {
    let Some(cell) = form.cell(id) else { return Ok(StdVec::new()) };
    let readers = form.readers(cell, Alloc::from(alloc())).map_err(|error| Broke::Harness(format!("the readers: {error}")))?;
    let mut out = StdVec::new();
    for reader in readers.iter() {
        out.push(form.saw(*reader, cell).map_err(|error| Broke::Harness(format!("a read: {error}")))?);
    }
    Ok(out)
}

/// The oracle after request `k`: the cells owed by generation alone, each explained, none after
/// a request that ran every task. Gives their count.
fn judge(k: u32, mem: &Answer<'_>, by_gen: &GenLast, ran_all: bool) -> Result<usize, Broke> {
    let (by_digest, by_generation) = (owed(mem)?, owed(by_gen)?);
    for id in by_digest.iter() {
        oracle(by_generation.binary_search(id).is_ok(), || format!("request {k}: {id:?} is owed by digest and settled by generation (a stale answer)"))?;
    }
    let mut extra = 0;
    for id in by_generation.iter().filter(|id| by_digest.binary_search(id).is_err()) {
        let (mem_saw, gen_saw) = (saws(mem, id)?, saws(by_gen, id)?);
        let explained = mem_saw.iter().all(|saw| *saw) && gen_saw.iter().any(|saw| !*saw);
        oracle(explained, || format!("request {k}: {id:?} is owed by generation and not explained (by digest {mem_saw:?}, by generation {gen_saw:?}): a cell that never settles"))?;
        extra += 1;
    }
    oracle(!ran_all || extra == 0, || format!("request {k}: {extra} cell(s) owed only by generation after a request that ran every task: they never settle"))?;
    Ok(extra)
}

/// The four requests; `plant` is the generation form's engine's bug. Gives the cells owed by
/// generation alone after each.
fn history(plant: Plant) -> Result<[usize; 4], Broke> {
    let steps: [(u8, bool); 4] = [(1, true), (2, false), (1, false), (1, true)];
    let (mut mem, mut by_gen) = (Answer::new_in(alloc()), GenLast::new(Bug::None));
    let mut counts = [0usize; 4];
    for (k, (text, reader)) in steps.iter().enumerate() {
        mem = one(mem, *text, *reader, Plant::None)?.answer;
        let finished = one(by_gen, *text, *reader, plant)?;
        by_gen = finished.answer;
        let k32 = u32::try_from(k + 1).unwrap_or(u32::MAX);
        counts[k] = judge(k32, &mem, &by_gen, *reader)?;
        let entered: StdVec<Name> = finished.entered.iter().copied().collect();
        println!("request {k32}: text {text}, reader run {reader}: owed only by generation {}, entered {entered:?}", counts[k]);
    }
    Ok(counts)
}

/// Expected: owed only by generation 0, 0, 1, 0 (request 3 the moved-back cell, settled in
/// request 4, which enters the reader again).
#[test]
fn a_moved_back_cell_is_owed_by_generation_for_one_request() {
    match history(Plant::None) {
        Ok(counts) => assert_eq!(counts, [0, 0, 1, 0], "the moved-back cell is owed by generation alone in request 3 and settles in request 4"),
        Err(broke) => panic!("{broke:?}"),
    }
}

/// Expected: request 4 keeps the cell owed though the reader ran: "they never settle".
#[test]
fn control_a_cell_that_never_settles_is_caught() {
    match history(Plant::StaleKeptOnTake) {
        Err(Broke::Oracle(why)) if why.contains("never settle") => println!("plant StaleKeptOnTake caught: {why}"),
        other => panic!("a cell kept owed after its reader ran again was not caught: {other:?}"),
    }
}
