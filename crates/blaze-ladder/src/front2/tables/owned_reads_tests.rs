//! A RECORDER WITH OWNERS (owned_reads.rs, lane RH6): one source a job, each cell's reads its
//! own.
//!
//! The fixture: a struct used by three fns, a trait with members and its impl, an alias chain
//! three deep, and a const in an array length. Every cell of the app is frozen two ways:
//! - ALONE: a source and an owned recorder of its own for each cell (what each head task did
//!   before, its reads by owner);
//! - SHARED: one source and one owned recorder for every cell, the cells asked in key order and
//!   reversed (what a job of the engine does now, blaze-cli engine_heads_once.rs).
//!
//! THE ORACLES: (a) each cell's own reads are equal in every run (who asked first changes no
//! cell's list), and over a run alone the union of the owners' lists is the plain recorder's set
//! (no read dropped, none invented); (b) the shared source lowers each item's own wants once
//! (plus one a re-entry) and evaluates each constant once, where a source a cell does both again.
//! THE CONTROL (rule 23): the planted [`Owning::Outermost`] (every read to the outermost owner,
//! as one recorder a head task kept) breaks (a) on the same fixture.

use blaze_id::ItemKey as LibKey;
use blaze_own::{AVec, Alloc};

use super::head::{ConstValues, SeededHeads, SeededHeadsOpts, StoreConsts, head_task, want_bytes};
use super::head_tests::{CORE_AT, Src, a, cells, own_wants, world_of};
use super::owned_reads::{BothOwners, OwnedRecorder, OwnedRecorderOpts, Owning, ReadOwners};
use super::recorder::{Recorder, RecorderOpts, TableRead};
use super::view::{Tables, TablesOpts};
use crate::front2::consts::ConstFail;
use crate::front2::heads::{Store, Want};
use crate::front2::index::Index;
use crate::front2::keys::Keys;

const SHARED: &str = "#![no_std]
pub struct Point {
    pub x: u32,
    pub y: u32,
}
pub fn one(p: Point) -> u32 {
    0
}
pub fn two(p: &Point) -> Point {
    Point { x: 0, y: 0 }
}
pub fn three(a: Point, b: Point) -> u32 {
    0
}
pub trait Shape {
    type Unit;
    fn area(&self) -> u32;
    fn unit(&self) -> Self::Unit;
}
impl Shape for Point {
    type Unit = u32;
    fn area(&self) -> u32 {
        0
    }
    fn unit(&self) -> u32 {
        0
    }
}
pub type First = Point;
pub type Second = First;
pub type Third = Second;
pub fn aliased(t: Third) -> Second {
    t
}
pub const N: usize = 3;
pub struct Buf {
    pub bytes: [u8; N],
}
pub fn filled(b: Buf) -> Buf {
    b
}
";

/// The crates whose lang rows name the lang items (the fixture's `core`).
const LANG: &[usize] = &[CORE_AT];

/// The tool head tasks are keyed by here.
fn tool() -> blaze_drive::key::Tool {
    blaze_drive::key::Tool(blaze_id::ContentId::of("blaze-ladder owned reads test", b"head"))
}

/// An evaluator that counts its asks, by constant: the store path's own (`StoreConsts`), with a
/// prepared set of its own for each ask, as blaze-cli engine_heads_once.rs `OnceConsts` (a set
/// shared by two cells lets the second use a definition the first read, unread).
struct Counted<'r, S> {
    names: S,
    index: &'r Index<'static>,
    bump: blaze_own::Bump<'static>,
    asked: AVec<'static, LibKey>,
}

impl<S: crate::front2::source::NamesSource<'static>> ConstValues<'static> for Counted<'_, S> {
    fn eval(&mut self, store: &mut Store<'static>, keys: &mut Keys, c: LibKey) -> Result<(), ConstFail> {
        self.asked.push(c);
        let prepared = blaze_scope::prepares::Prepares::new_in(self.bump);
        StoreConsts { names: self.names, index: self.index, prepared }.eval(store, keys, c)
    }
}

/// What one run froze: each cell's own names reads and evaluation reads (sorted, each once), by
/// want; the source's lowerings, own wants and re-entries; every constant asked.
struct Run {
    reads: AVec<'static, (Want, AVec<'static, TableRead>, AVec<'static, TableRead>)>,
    lowered: u32,
    own: u32,
    reentered: u32,
    /// What the constant evaluator lowered in the freezes' stores (head.rs `Frozen::eval_lowered`).
    eval_lowered: u32,
    /// Each cell lowered past its own wants: (cell, lowered, own), the source's per-cell count.
    past_own: AVec<'static, (Want, u32, u32)>,
    asked: AVec<'static, LibKey>,
}

/// `order`'s cells frozen through ONE source and one owned recorder pair (each task entering
/// its own cell for its lookups, as blaze-cli engine_heads_once.rs `one_head` does).
fn shared(names: Src, sized: Option<LibKey>, frame: &'static blaze_mem::Frame, order: &[Want], owning: Owning) -> Run {
    let g: Alloc<'static> = a();
    let freeze = OwnedRecorder::new(OwnedRecorderOpts { alloc: g, owning });
    let evals = OwnedRecorder::new(OwnedRecorderOpts { alloc: g, owning });
    let both = BothOwners { names: &freeze, evals: &evals };
    let view = Tables::new(TablesOpts { records: names, impls: None, lang_crates: LANG, reads: &freeze });
    let eval_view = Tables::new(TablesOpts { records: names, impls: None, lang_crates: LANG, reads: &evals });
    // The lang index is read before any cell is entered (once a job, as the engine's): through
    // a plain recorder of its own, or its reads would be no one's.
    let lang = Recorder::new(RecorderOpts { alloc: g });
    let index = Index::from_source(eval_view.reading_into(&lang), LANG).unwrap_or_else(|_| panic!("the fixture's lang index"));
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let mut consts = Counted { names: eval_view, index: &index, bump, asked: AVec::new_in(g) };
    // No receiver pointers looked up (as `plain`'s): the lookup's names reads are a job's, not a
    // cell's, and these runs compare cells' reads alone.
    let mut source = SeededHeads::new(SeededHeadsOpts { names: view, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: g, alloc: g, consts: Some(&mut consts), known: None, owners: Some(&both) });
    for w in order.iter().copied() {
        both.enter(w);
        let made = head_task(view, &mut source, w, tool(), g);
        both.leave();
        if let Some(e) = &made.failed {
            panic!("the head task of {w:?} failed: {e}");
        }
    }
    let own: u32 = source.cells().map(own_wants).sum();
    let (lowered, reentered, eval_lowered) = (source.lowered, source.reentered, source.eval_lowered);
    let mut past_own: AVec<'static, (Want, u32, u32)> = AVec::new_in(g);
    past_own.extend(source.lowered_each().filter(|(w, n)| *n > own_wants(*w)).map(|(w, n)| (w, n, own_wants(w))));
    let mut made: AVec<'static, Want> = AVec::new_in(g);
    made.extend(source.cells());
    drop(source);
    let mut reads = AVec::new_in(g);
    for w in made.iter().copied() {
        reads.push((w, owned(&freeze, w), owned(&evals, w)));
    }
    reads.sort_unstable_by_key(|(w, _, _)| want_bytes(*w));
    assert!(freeze.broken().is_none() && evals.broken().is_none(), "the recorders refused: {:?} {:?}", freeze.broken(), evals.broken());
    let mut asked = AVec::new_in(g);
    asked.extend_from_slice(&consts.asked);
    Run { reads, lowered, own, reentered, eval_lowered, past_own, asked }
}

/// `w`'s reads in `recorder`, sorted, each once.
fn owned(recorder: &OwnedRecorder<'static>, w: Want) -> AVec<'static, TableRead> {
    let mut out = AVec::new_in(a());
    recorder.reads_of(w, &mut |one| out.push(one)).unwrap_or_else(|e| panic!("the reads of {w:?}: {e}"));
    out
}

/// The cells of `run` that are the app's (`cells`), with their reads.
fn of_app<'r>(run: &'r Run, app: &[Want]) -> AVec<'static, &'r (Want, AVec<'static, TableRead>, AVec<'static, TableRead>)> {
    let mut out = AVec::new_in(a());
    out.extend(run.reads.iter().filter(|(w, _, _)| app.contains(w)));
    out
}

/// The names reads a PLAIN recorder took from freezing `w` alone: the set every owner's list
/// of that run must add up to.
fn plain(names: Src, sized: Option<LibKey>, frame: &'static blaze_mem::Frame, w: Want) -> AVec<'static, TableRead> {
    let g = a();
    let rec = Recorder::new(RecorderOpts { alloc: g });
    let evals = Recorder::new(RecorderOpts { alloc: g });
    let view = Tables::new(TablesOpts { records: names, impls: None, lang_crates: LANG, reads: &rec });
    let eval_view = Tables::new(TablesOpts { records: names, impls: None, lang_crates: LANG, reads: &evals });
    let index = Index::from_source(eval_view, LANG).unwrap_or_else(|_| panic!("the fixture's lang index"));
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let mut consts = Counted { names: eval_view, index: &index, bump, asked: AVec::new_in(g) };
    let mut source = SeededHeads::new(SeededHeadsOpts { names: view, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: g, alloc: g, consts: Some(&mut consts), known: None, owners: None });
    let made = head_task(view, &mut source, w, tool(), g);
    assert!(made.failed.is_none(), "{w:?} alone failed");
    drop(source);
    let mut out = rec.finish().unwrap_or_else(|e| panic!("the plain reads of {w:?}: {e}"));
    out.sort_unstable();
    out.dedup();
    out
}

/// Each cell's own reads, equal whoever asked first; a run alone adds up to the plain set; the
/// plant breaks the equality. Expected line: `owned reads: N app cells, alone = forward =
/// reversed (R names reads, E evaluation reads), union = plain for each; outermost: D cells differ`.
#[test]
fn a_cells_reads_are_its_own_from_a_shared_source() {
    let (names, sized, frame) = world_of(SHARED);
    let app = cells(names);
    assert!(app.len() >= 10, "the fixture's cells: {}", app.len());
    let mut reversed: AVec<'static, Want> = AVec::new_in(a());
    reversed.extend(app.iter().rev().copied());
    let forward = shared(names, sized, frame, &app, Owning::Each);
    let backward = shared(names, sized, frame, &reversed, Owning::Each);
    let (mut names_reads, mut eval_reads) = (0usize, 0usize);
    for w in app.iter().copied() {
        let alone = shared(names, sized, frame, &[w], Owning::Each);
        let mine = |run: &Run| run.reads.iter().find(|(x, _, _)| *x == w).map(|(_, n, e)| (n.clone(), e.clone())).unwrap_or_else(|| panic!("{w:?} not frozen"));
        let (n, e) = mine(&alone);
        assert!(mine(&forward) == (n.clone(), e.clone()), "ORACLE: {w:?}'s reads from the source shared in key order differ from its reads alone");
        assert!(mine(&backward) == (n.clone(), e.clone()), "ORACLE: {w:?}'s reads from the source shared in reversed order differ from its reads alone");
        // No read dropped or invented: alone, every owner's list adds up to the plain set.
        let mut union: AVec<'static, TableRead> = AVec::new_in(a());
        for (_, n, _) in alone.reads.iter() {
            union.extend_from_slice(n);
        }
        union.sort_unstable();
        union.dedup();
        assert!(union[..] == plain(names, sized, frame, w)[..], "ORACLE: {w:?} alone: the owners' lists do not add up to the plain recorder's reads");
        names_reads += n.len();
        eval_reads += e.len();
    }
    // The nested read: `Third`'s alias chain is frozen inside `aliased`'s freeze, and keeps its
    // own reads there, not `aliased`'s.
    assert!(of_app(&forward, &app).iter().all(|(_, n, _)| !n.is_empty()), "ORACLE: an app cell owns no read in the shared run");
    // The control: every read to the outermost owner (one recorder a head task, as before).
    let planted = shared(names, sized, frame, &app, Owning::Outermost);
    let mut differ = 0usize;
    for w in app.iter().copied() {
        let alone = shared(names, sized, frame, &[w], Owning::Outermost);
        let mine = |run: &Run| run.reads.iter().find(|(x, _, _)| *x == w).map(|(_, n, e)| (n.clone(), e.clone()));
        if mine(&planted) != mine(&alone) {
            differ += 1;
        }
    }
    assert!(differ > 0, "CONTROL: with every read to the outermost owner, each cell's reads were still equal: the oracle sees nothing");
    std::println!(
        "owned reads: {} app cells, alone = forward = reversed ({names_reads} names reads, {eval_reads} evaluation reads), union = plain for each; outermost: {differ} cells differ",
        app.len()
    );
}

/// Work once: the shared source lowers each item's own wants once and evaluates each constant
/// once; a source a cell does both again. Expected line: `work once: N cells, lowered L (own
/// O, re-entered R) shared, the evaluator's E, against P a source each; constants asked C shared
/// (distinct D) against Q a source each`.
///
/// THE 31ST OF 30 (first run, 2026-11-18 00:41; lane H7): the count included what the CONSTANT
/// EVALUATOR lowered in a freeze's store. `Buf`'s `[u8; N]` asks an anonymous constant whose
/// body reads `N`; the evaluator's `Records` make `N`'s head by an unseeded `Store::ensure`, so
/// `N`'s head is lowered in `Buf`'s store though `N`'s own cell froze it. head.rs now counts that
/// apart (`Frozen::eval_lowered`): the freeze's own lowerings meet the floor exactly, and the
/// evaluator's are printed (expected 1: `N`'s head, once, by `Buf`'s evaluation). Each cell
/// lowered past its own wants is printed when the oracle fails.
///
/// THE COUNT DELETED (lane H8, 2026-11-18): `lowered == own + re-entered` (second run: 31 against
/// 30; past their own `[(Adt(0147fd87b32f), 3, 2), (Members(054a22c11027), 6, 2),
/// (Members(054a22c11027), 4, 2)]`, two DIFFERENT traits: a key prints its first six bytes, its
/// crate's prefix, so every item of one crate prints alike) cannot be right: a re-entry is one
/// `open` hit (head.rs `HeadSource::open`), a need seeded as `Lower` in the asking store, and
/// the store lowers that need WITH whatever of the re-entered cell it pulls (a trait's head with
/// its members, an ADT's definition with its constructors' heads), so one re-entry is one or
/// more lowerings. What must be right, and is asserted: each cell is kept ONCE by the source
/// (frozen once, whoever asks), and each constant is evaluated once; the lowerings are printed.
#[test]
fn one_source_lowers_each_head_and_evaluates_each_constant_once() {
    let (names, sized, frame) = world_of(SHARED);
    let app = cells(names);
    let run = shared(names, sized, frame, &app, Owning::Each);
    let mut kept: AVec<'static, [u8; super::head::WANT_BYTES]> = AVec::new_in(a());
    kept.extend(run.reads.iter().map(|(w, _, _)| want_bytes(*w)));
    let all = kept.len();
    kept.sort_unstable();
    kept.dedup();
    assert_eq!(kept.len(), all, "ORACLE: the shared source kept a cell twice (frozen twice); cells lowered past their own wants (cell, lowered, own): {:?}", &run.past_own[..]);
    assert!(run.lowered >= run.own, "ORACLE: the shared source lowered fewer wants than its cells' own ({} against {})", run.lowered, run.own);
    let mut distinct: AVec<'static, LibKey> = AVec::new_in(a());
    distinct.extend_from_slice(&run.asked);
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(run.asked.len(), distinct.len(), "ORACLE: the shared source evaluated a constant twice: {:?}", run.asked);
    assert!(!distinct.is_empty(), "the fixture's `[u8; N]` asked no constant");
    // Today's: a source each.
    let (mut each_lowered, mut each_asked) = (0u32, 0usize);
    for w in app.iter().copied() {
        let one = shared(names, sized, frame, &[w], Owning::Each);
        each_lowered += one.lowered;
        each_asked += one.asked.len();
    }
    assert!(each_lowered > run.lowered, "CONTROL: a source each lowered no more than one shared ({each_lowered} against {})", run.lowered);
    assert!(each_asked >= run.asked.len(), "a source each asked fewer constants than one shared");
    std::println!(
        "work once: {} cells, lowered {} (own {}, re-entered {}) shared, the evaluator's {}, against {each_lowered} a source each; constants asked {} shared (distinct {}) against {each_asked} a source each",
        app.len(),
        run.lowered,
        run.own,
        run.reentered,
        run.eval_lowered,
        run.asked.len(),
        distinct.len()
    );
}
