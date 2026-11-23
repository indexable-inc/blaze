//! A file found while the request runs (engine.rs invariant 14, [`Engine::plan_file`]): a
//! crate's names find a module file no plan named (a macro's `mod $n;`), the driver outlines it
//! and serves the outline to the names, and the engine plans the file between phases. The model:
//!
//! - `A` and `B` (Outlines): two files' outlines, each defining its whole cell from its text.
//! - `C` (Names): the crate's names, in `A`'s plan, reading `A`'s cell and `B`'s.
//! - `AB`, `BB` (Bodies): a body of each file, reading its file's cell.
//!
//! LITERAL: `B` is in the edit (the walk found it, a `mod b;` written plainly). FOUND: the edit
//! holds `A` alone; `C` reads the outline served to it, and after the names' merge the driver
//! calls `plan_file(B, ..)`. The claims, each its test:
//!
//! - FOUND EQUALS LITERAL: the two answers' rows, `B`'s plan and the stored pages' root are
//!   equal, `B`'s row is a planned row (not inline), and `C` is entered once. CONTROL (rule 23):
//!   the same request with `plan_file` left out gives other rows.
//! - REPLAY: the next request planning both files (the walk seeded from the kept places) enters
//!   nothing.
//! - AN EDIT OF THE FOUND FILE enters exactly `B`, its body and the names that read it.
//! - PLANNED AGAIN: a found file in the edit (from its kept place) whose plan the names narrow
//!   is given again with no outline: the task it no longer plans is retired, nothing of the file
//!   entered.
//! - DROPPED: a found file no names read now ([`Engine::drop_file`]) leaves no row and no plan.
//! - REFUSALS: a task of a phase already taken is [`EngineError::FiledLate`]; a file the edit
//!   planned is [`EngineError::FiledTwice`]; a call with a phase open is
//!   [`EngineError::OutOfTurn`]; a file planned again that no request planned is
//!   [`EngineError::FiledUnplanned`].

use std::collections::BTreeMap;
use std::println;
use std::vec::Vec as StdVec;

use blaze_drive::cells::Cell;
use blaze_drive::key::{self, Key, Kind, Name, Tool};
use blaze_id::ContentId;
use blaze_own::Vec;

use crate::engine::{Answer, Buffer, CellId, Done, Edit, Engine, EngineError, Everything, Finished, Phase, Plant, Planned, Pool, SVec, Stage, Stands, Task, Tools, View};
use crate::last::LastAnswer;
use crate::pages::Kept;
use crate::sim::{Pools, alloc};

const TOOL: Tool = Tool(ContentId([111; 32]));
const A: Name = Name::new(Kind::FileItems, ContentId([112; 32]));
const B: Name = Name::new(Kind::FileItems, ContentId([113; 32]));
const C: Name = Name::new(Kind::CrateScope, ContentId([114; 32]));
const AB: Name = Name::new(Kind::BodyCheck, ContentId([115; 32]));
const BB: Name = Name::new(Kind::BodyCheck, ContentId([116; 32]));
const WHOLE: Cell = Cell([1; 16]);
const TOOLS: Tools = Tools([TOOL; key::KINDS]);

fn whole(of: Name) -> CellId {
    CellId { of, cell: WHOLE }
}

/// Each task's text: what its source, and so its key, is made from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Texts {
    a: u8,
    b: u8,
}

impl Texts {
    fn text(&self, name: Name) -> u8 {
        match name {
            n if n == A || n == AB => self.a,
            n if n == B || n == BB => self.b,
            _ => 0,
        }
    }

    fn source(&self, name: Name) -> ContentId {
        key::digest("plan_file_sim source", &[name.subject.0[0], self.text(name)])
    }

    fn planned(&self, name: Name, stage: Stage, file: Name) -> Planned {
        Planned { name, phase: Phase::new(stage, 0), source: self.source(name), file }
    }

    fn plan_a(&self) -> [Planned; 3] {
        [self.planned(A, Stage::Outlines, A), self.planned(C, Stage::Names, A), self.planned(AB, Stage::Bodies, A)]
    }

    fn plan_b(&self) -> [Planned; 2] {
        [self.planned(B, Stage::Outlines, B), self.planned(BB, Stage::Bodies, B)]
    }

    /// An outline's digest: a function of its file's text alone.
    fn outline_out(&self, file: Name) -> ContentId {
        key::digest("plan_file_sim outline", &[file.subject.0[0], self.text(file)])
    }

    /// The outline of `file`, as the driver makes it for a file found while the request runs.
    fn outline(&self, file: Name) -> Done<'static> {
        let out = self.outline_out(file);
        let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(alloc());
        defines.push((WHOLE, out));
        Done { name: file, key: TOOLS.key(file, &self.source(file)), out, defines, reads: Vec::new_in(alloc()), findings: 0, failed: false }
    }
}

/// How a request plans `B`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plans {
    /// In the edit from the start.
    Literal,
    /// Found by `C`, planned after the names' merge.
    Found,
    /// THE CONTROL: found by `C` and never planned.
    Skipped,
    /// Found, and its body put in the names' phase, already taken.
    Late,
    /// `A`, which the edit planned, given again.
    Twice,
    /// Found, and planned while the names' phase is open.
    WhileOpen,
    /// In the edit (its kept place), and planned again after the names with its outline alone.
    Narrowed,
    /// In the edit (its kept place), and dropped after the names, which no longer read it.
    Dropped,
    /// Found, and planned again (no outline) though no request planned it.
    Unplanned,
}

/// How `C` reads `B`: as the view has it (the walk planned `B`), the digest of the outline
/// served to it (the driver outlined the file it needed), or not at all (its macro no longer
/// writes the `mod`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadB {
    View,
    Served,
    Not,
}

/// The model's tasks.
fn work<L: LastAnswer>(texts: &Texts, read_b: ReadB, view: View<'_, 'static, L>, task: &Task, buffer: &mut Buffer<'static>) {
    let a = alloc();
    let name = task.planned.name;
    let mut reads: SVec<'static, (CellId, ContentId)> = Vec::new_in(a);
    let mut defines: SVec<'static, (Cell, ContentId)> = Vec::new_in(a);
    let mut parts: StdVec<u8> = StdVec::from([name.subject.0[0], texts.text(name)]);
    let out = match name {
        n if n == A || n == B => {
            let out = texts.outline_out(n);
            defines.push((WHOLE, out));
            buffer.push(Done { name, key: task.key, out, defines, reads, findings: 0, failed: false }, Stands::ByReads);
            return;
        }
        n if n == C => {
            let seen_a = view.cell(&whole(A));
            reads.push((whole(A), seen_a));
            parts.extend_from_slice(&seen_a.0);
            let seen_b = match read_b {
                ReadB::Served => Some(texts.outline_out(B)),
                ReadB::View => Some(view.cell(&whole(B))),
                ReadB::Not => None,
            };
            if let Some(seen_b) = seen_b {
                reads.push((whole(B), seen_b));
                parts.extend_from_slice(&seen_b.0);
            }
            key::digest("plan_file_sim names", &parts)
        }
        n => {
            let file = if n == AB { A } else { B };
            let seen = view.cell(&whole(file));
            reads.push((whole(file), seen));
            parts.extend_from_slice(&seen.0);
            key::digest("plan_file_sim body", &parts)
        }
    };
    defines.push((WHOLE, out));
    buffer.push(Done { name, key: task.key, out, defines, reads, findings: 0, failed: false }, Stands::ByReads);
}

/// One request over `last`: the edit holds `A`'s plan (and `B`'s when `files_b`), and `B` is
/// planned as `plans` says.
fn request(last: Answer<'static>, texts: Texts, files_b: bool, plans: Plans) -> Result<Finished<'static>, EngineError> {
    let plan_a = texts.plan_a();
    let plan_b = texts.plan_b();
    let both: [(Name, &[Planned]); 2] = [(A, &plan_a[..]), (B, &plan_b[..])];
    let files = match files_b || plans == Plans::Literal {
        true => &both[..],
        false => &both[..1],
    };
    let read_b = match plans {
        Plans::Dropped => ReadB::Not,
        _ if !files_b && plans != Plans::Literal => ReadB::Served,
        _ => ReadB::View,
    };
    let mut engine = Engine::new_in(TOOLS, last, &Edit { files }, Everything, Plant::None, alloc())?;
    let mut buffers: StdVec<Buffer<'static>> = StdVec::from([Buffer::new_in(alloc())]);
    let mut planned_once = false;
    while let Some(phase) = engine.next_phase()? {
        {
            let view = engine.view();
            let tasks = engine.tasks();
            Pools::One.run(tasks, &mut buffers, &|_: &(), task: &Task, buffer: &mut Buffer<'static>| work(&texts, read_b, view, task, buffer));
        }
        if phase.stage == Stage::Names && plans == Plans::WhileOpen {
            engine.plan_file(B, Some(texts.outline(B)), &plan_b)?;
        }
        engine.merge(&mut buffers)?;
        if phase.stage != Stage::Names || planned_once {
            continue;
        }
        planned_once = true;
        match plans {
            Plans::Found if !files_b => engine.plan_file(B, Some(texts.outline(B)), &plan_b)?,
            Plans::Late => {
                let late = [plan_b[0], texts.planned(BB, Stage::Names, B)];
                engine.plan_file(B, Some(texts.outline(B)), &late)?;
            }
            Plans::Twice => engine.plan_file(A, Some(texts.outline(A)), &plan_a)?,
            Plans::Narrowed => engine.plan_file(B, None, &plan_b[..1])?,
            Plans::Dropped => engine.drop_file(B)?,
            Plans::Unplanned => engine.plan_file(B, None, &plan_b)?,
            Plans::Found | Plans::Literal | Plans::Skipped | Plans::WhileOpen => {}
        }
    }
    engine.finish()
}

/// A row as compared: everything but the request that made it.
type Seen = (Key, Phase, Name, ContentId, StdVec<(Cell, ContentId)>, StdVec<(CellId, ContentId)>, u32, bool, bool);

fn seen(answer: &Answer<'_>) -> BTreeMap<Name, Seen> {
    answer
        .rows
        .iter()
        .map(|(name, row)| {
            (*name, (row.key, row.phase, row.file, row.out, row.defines.iter().copied().collect(), row.reads.iter().copied().collect(), row.findings, row.inline, row.failed))
        })
        .collect()
}

fn plan_of(answer: &Answer<'_>, file: Name) -> Option<StdVec<Name>> {
    answer.by_file.get(&file).map(|names| names.iter().copied().collect())
}

/// The answer and its stored pages' root (pages.rs), the answer handed back for the next request.
fn stored(finished: Finished<'static>) -> (Answer<'static>, Key) {
    let kept = Kept::after(blaze_own::BTreeMap::new_in(alloc()), finished.answer, &finished.causes, blaze_own::BTreeMap::new_in(alloc()));
    let root = crate::pages::encode(&kept, None, alloc()).root;
    (kept.answer, root)
}

fn fresh(texts: Texts, plans: Plans) -> Finished<'static> {
    match request(Answer::new_in(alloc()), texts, false, plans) {
        Ok(finished) => finished,
        Err(error) => panic!("the {plans:?} request was refused: {error}"),
    }
}

const TEXTS: Texts = Texts { a: 1, b: 2 };

/// FOUND EQUALS LITERAL: the found file's row, its plan and the stored pages are the bytes the
/// walk's plan of it gives; `C` is entered once (the outline it was served is the one filed).
#[test]
fn a_found_file_is_planned_as_the_walk_plans_it() {
    let literal = fresh(TEXTS, Plans::Literal);
    let found = fresh(TEXTS, Plans::Found);
    let (rows_l, rows_f) = (seen(&literal.answer), seen(&found.answer));
    println!("literal rows {:?}\nfound rows {:?}", rows_l.keys().collect::<StdVec<_>>(), rows_f.keys().collect::<StdVec<_>>());
    assert_eq!(rows_l, rows_f, "the found file's rows are the literal plan's");
    assert!(rows_f.get(&B).is_some_and(|row| !row.7), "B's row is a planned row, never an inline one");
    assert_eq!(plan_of(&literal.answer, B), plan_of(&found.answer, B), "B's plan is the literal plan's");
    assert_eq!(found.entered.iter().filter(|name| **name == C).count(), 1, "the names are entered once: {:?}", found.entered);
    let ((_, root_l), (_, root_f)) = (stored(literal), stored(found));
    assert_eq!(root_l, root_f, "the stored pages are the same bytes");
}

/// THE CONTROL (rule 23): the file found and never planned gives another answer (or a refusal),
/// so the equality above is `plan_file`'s.
#[test]
fn without_plan_file_the_found_file_is_missing() {
    let literal = seen(&fresh(TEXTS, Plans::Literal).answer);
    match request(Answer::new_in(alloc()), TEXTS, false, Plans::Skipped) {
        Ok(skipped) => {
            let skipped = seen(&skipped.answer);
            assert!(!skipped.contains_key(&B) && !skipped.contains_key(&BB), "no row of the file nobody planned");
            assert_ne!(skipped, literal, "the control differs from the literal plan");
        }
        Err(_error) => {}
    }
}

/// REPLAY: the next request plans both files (the walk seeded from the kept places) and enters
/// nothing; AN EDIT of the found file enters exactly its outline, its body and the names that
/// read it.
#[test]
fn a_found_file_replays_and_its_edit_enters_its_own() {
    let (last, _) = stored(fresh(TEXTS, Plans::Found));
    let replay = match request(last, TEXTS, true, Plans::Found) {
        Ok(replay) => replay,
        Err(error) => panic!("the replay was refused: {error}"),
    };
    assert_eq!(replay.report.entered, 0, "the replay enters nothing: {:?}", replay.entered);
    let (last, _) = stored(replay);
    let edited = Texts { a: TEXTS.a, b: TEXTS.b + 2 };
    let after = match request(last, edited, true, Plans::Found) {
        Ok(after) => after,
        Err(error) => panic!("the edit was refused: {error}"),
    };
    let mut entered: StdVec<Name> = after.entered.iter().copied().collect();
    entered.sort_unstable();
    let mut want = StdVec::from([B, C, BB]);
    want.sort_unstable();
    assert_eq!(entered, want, "the edit of the found file enters its own tasks and its readers");
}

/// REFUSALS, each named: a task of a phase taken, a file planned twice, a call mid-phase.
#[test]
fn plan_file_refuses_by_name() {
    match request(Answer::new_in(alloc()), TEXTS, false, Plans::Late) {
        Err(EngineError::FiledLate { file, name, .. }) => assert_eq!((file, name), (B, BB), "the late task is named"),
        Err(error) => panic!("refused for another reason: {error}"),
        Ok(_) => panic!("a task of a phase already taken was planned"),
    }
    match request(Answer::new_in(alloc()), TEXTS, false, Plans::Twice) {
        Err(EngineError::FiledTwice { file }) => assert_eq!(file, A, "the file is named"),
        Err(error) => panic!("refused for another reason: {error}"),
        Ok(_) => panic!("a file planned twice was taken"),
    }
    match request(Answer::new_in(alloc()), TEXTS, false, Plans::WhileOpen) {
        Err(EngineError::OutOfTurn) => {}
        Err(error) => panic!("refused for another reason: {error}"),
        Ok(_) => panic!("a file was planned with a phase open"),
    }
    match request(Answer::new_in(alloc()), TEXTS, false, Plans::Unplanned) {
        Err(EngineError::FiledUnplanned { file }) => assert_eq!(file, B, "the file is named"),
        Err(error) => panic!("refused for another reason: {error}"),
        Ok(_) => panic!("a file no request planned was planned again with no outline"),
    }
}

/// PLANNED AGAIN: the found file in the edit from its kept place; the names entered again (their
/// crate's other file moved: with nothing moved no names' phase opens, and nothing is planned
/// again), its plan narrowed after them to its outline: its body's row is retired, its outline's
/// stands, and only the moved file's tasks are entered, none of the found file's.
#[test]
fn a_found_file_planned_again_retires_what_it_no_longer_plans() {
    let (last, _) = stored(fresh(TEXTS, Plans::Found));
    let moved = Texts { a: TEXTS.a + 2, b: TEXTS.b };
    let narrowed = match request(last, moved, true, Plans::Narrowed) {
        Ok(narrowed) => narrowed,
        Err(error) => panic!("the narrowed plan was refused: {error}"),
    };
    let rows = seen(&narrowed.answer);
    assert!(rows.contains_key(&B) && !rows.contains_key(&BB), "the outline's row stands and the body's is retired: {:?}", rows.keys().collect::<StdVec<_>>());
    assert_eq!(plan_of(&narrowed.answer, B), Some(StdVec::from([B])), "the file plans its outline alone");
    let mut entered: StdVec<Name> = narrowed.entered.iter().copied().collect();
    entered.sort_unstable();
    let mut want = StdVec::from([A, C, AB]);
    want.sort_unstable();
    assert_eq!(entered, want, "planned again, nothing of the found file is entered");
}

/// DROPPED: the names entered again (their crate's other file moved) and no longer read the
/// found file: dropped, it leaves no row and no plan, as a file the edit removes.
#[test]
fn a_found_file_no_names_read_is_dropped() {
    let (last, _) = stored(fresh(TEXTS, Plans::Found));
    let moved = Texts { a: TEXTS.a + 2, b: TEXTS.b };
    let dropped = match request(last, moved, true, Plans::Dropped) {
        Ok(dropped) => dropped,
        Err(error) => panic!("the drop was refused: {error}"),
    };
    let rows = seen(&dropped.answer);
    assert!(!rows.contains_key(&B) && !rows.contains_key(&BB), "no row of the dropped file: {:?}", rows.keys().collect::<StdVec<_>>());
    assert!(plan_of(&dropped.answer, B).is_none_or(|plan| plan.is_empty()), "no plan of the dropped file");
    assert!(dropped.entered.contains(&C), "the names were entered: {:?}", dropped.entered);
}
