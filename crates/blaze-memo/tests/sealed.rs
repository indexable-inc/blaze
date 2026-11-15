//! A sealed arena dropped under deterministic simulation (rules 43, 64): since 2026-11-14
//! `Memo::next` takes a sealed arena (`Inputs::seal`: a crate whose sources are pinned, std's)
//! in its changed set and drops, to a fixpoint, every arena that declared or read it. What is
//! at stake: an id of a dead arena is refused by name (`Memo::slot`), but a kept `&'db` value,
//! a read list's callees, a trie or a column head are raw pointers with no generation check,
//! so one borrow missed is a use-after-free, not a panic.
//!
//! The world, on the real attributes: arena 0 the session; 1 = R, open and (drawn) sealed,
//! holding leaves; 2 = S, sealed when R is (drawn), declaring 0 and R; 3 = U, declaring 0 and
//! S; 4 = V, declaring only 0, whose input holds R's crate id (made with it, as base-db hands
//! out crate ids with no read) and whose query asks a `returns(ref)` query homed in R, keeps
//! the reference, and interns a value homed in R. Reads are kept (`Reads::Kept`).
//!
//! A case, from one tape: steps, each of asks of S, U and V on the current snapshot; an edit of
//! R, S, U, V or nothing; `Memo::next` (in some steps called while V's query is paused inside
//! its run on the old snapshot, through `Db::pause`, the seam `sim.rs`'s baton uses); the
//! rebuild of what it dropped; the old snapshot dropped, so the memory of every arena only it
//! held is freed; asks again. Single-threaded and a function of the tape, so a failure's tape
//! replays.

#![feature(allocator_api)]

mod common;

use std::{
    alloc::Global,
    fmt::Write as _,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use blaze_core::lock::Lock;
use blaze_mem::Tag;
use blaze_memo::{
    Alone, ArenaId, Builder, Config, Database, Fact, Gid, Id, Memo, Plant, Read, Reads, record,
};
use blaze_sim::{Broke, Tape};

use common::{Crate, Db, LEAVES, Leaf, crate_fact, facts_of, leaf_fact};

const SESSION: u32 = ArenaId::SESSION_INDEX;
const R: u32 = 1;
const S: u32 = 2;
const U: u32 = 3;
const V: u32 = 4;
const ARENAS: u32 = 5;
const NAMES: [&str; ARENAS as usize] = ["the session", "R", "S", "U", "V"];
/// What a step edits, calm first.
const EDITS: [Option<u32>; 5] = [None, Some(R), Some(S), Some(U), Some(V)];
/// The most steps of a case.
const STEPS: usize = 3;
/// V's key in the ask a step pauses, plus the step: asked once, so it runs (a hit would not
/// pause).
const PAUSED: u32 = 1000;

// The queries.

/// A crate's own leaf values, homed at the crate's arena: what V keeps a reference into.
#[blaze_memo::tracked(returns(ref))]
fn values_of(db: &dyn Db, krate: Crate) -> [u64; LEAVES] {
    let mut out = [0; LEAVES];
    for (at, leaf) in krate.leaves(db).iter().enumerate() {
        out[at] = *leaf.n(db);
    }
    out
}

/// A crate's values and, three times over, its dependencies' totals: S over R, U over S.
#[blaze_memo::tracked(returns(copy))]
fn total_of(db: &dyn Db, krate: Crate) -> u64 {
    let mut sum = values_of(db, krate).iter().fold(0_u64, |sum, n| sum.wrapping_add(*n));
    for dep in krate.deps(db) {
        sum = sum.wrapping_add(total_of(db, *dep).wrapping_mul(3));
    }
    sum
}

/// A value of R's, interned (homed at R, its leaf's arena) by V's query.
#[blaze_memo::interned(debug)]
pub struct RTag<'db> {
    pub leaf: Leaf,
    pub n: u64,
}

/// What V's query keeps: a reference into R's arena, an id of R's arena, and its own sum.
#[derive(Clone, Copy)]
pub struct View<'db> {
    /// `values_of(R)`'s value, where it lives: in R's arena.
    pub values: &'db [u64; LEAVES],
    pub tag: RTag<'db>,
    pub own: u64,
    pub key: u32,
}

/// V's query: R reached through the crate id V's input holds (V declared no borrow of R), so
/// the only borrow that ties V to R is the one its reads note. Pauses before it reads R and
/// after.
#[blaze_memo::tracked(returns(copy))]
fn view<'db>(db: &'db dyn Db, v: Crate, which: u32) -> View<'db> {
    db.pause();
    let r = v.deps(db)[0];
    let values = values_of(db, r);
    let tag = RTag::new(db, r.leaves(db)[0], values[0]);
    db.pause();
    let own = v.leaves(db).iter().fold(0_u64, |sum, leaf| sum.wrapping_add(*leaf.n(db)));
    View { values, tag, own, key: which }
}

// The database: the driver's step at a pause of V's run.

/// When the driver calls `Memo::next`: at the `at`-th pause of V's run, once.
struct Pause {
    at: u32,
    seen: u32,
}

struct SealDb<'a> {
    memo: &'a Memo,
    pause: &'a Lock<Option<Pause>>,
    driver: &'a Lock<Driver>,
}

impl Database for SealDb<'_> {
    fn memo(&self) -> &Memo {
        self.memo
    }
}

impl Db for SealDb<'_> {
    /// At the armed pause, the driver makes the next snapshot from this one (the old), while
    /// this run is inside V's query on it.
    fn pause(&self) {
        let mut pause = self.pause.lock();
        let fire = match &mut *pause {
            Some(it) => {
                it.seen += 1;
                it.seen == it.at
            }
            None => false,
        };
        if fire {
            *pause = None;
        }
        drop(pause);
        if fire {
            let mut driver = self.driver.lock();
            if driver.pause_at == Some(2) {
                // The run read R before this pause (a dead id would have ended it there), in
                // the snapshot that owns V's arena: its borrow is noted before the freeze.
                driver.v_read_r = true;
            }
            let made = driver.advance(self.memo);
            driver.made = Some(made);
        }
    }

    fn ran(&self, _: u8, _: Gid, _: u32) {}
}

// The driver and its model.

/// One case: the seals, the first values, and the steps.
#[derive(Clone, Debug)]
struct Script {
    r_sealed: bool,
    s_sealed: bool,
    values: [[u64; LEAVES]; ARENAS as usize],
    steps: Vec<Step, Global>,
}

#[derive(Clone, Debug)]
struct Step {
    asks: Vec<Ask, Global>,
    /// `Memo::next` is called at this pause (1, before V reads R; 2, after) of a run of V on
    /// the old snapshot.
    paused: Option<u32>,
    edit: Option<u32>,
    again: Vec<Ask, Global>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    S,
    U,
    V(u32),
}

#[derive(Clone, Copy, Debug)]
struct Ask {
    who: Who,
    record: bool,
}

/// How the arenas are sealed in a case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sealing {
    /// R sealed by a draw, S sealed by another where R is.
    Drawn,
    Sealed,
    Unsealed,
}

/// What a case got to, so a pass shows the oracle ran on it.
#[derive(Clone, Copy, Debug, Default)]
struct Seen {
    /// A sealed arena named changed and dropped.
    sealed_named: u32,
    /// V dropped with R by the borrow its read noted alone.
    dropped_by_read: u32,
    /// `Memo::next` called while V's run was paused on the old snapshot.
    paused: u32,
    /// A carried V's dead id of R refused by name.
    refused: u32,
}

#[derive(Clone, Copy, Debug)]
struct Edit {
    arena: u32,
    leaf: usize,
    by: u64,
}

/// What the driver knows and the model of what the memo must do.
struct Driver {
    plant: Plant,
    r_sealed: bool,
    s_sealed: bool,
    values: [[u64; LEAVES]; ARENAS as usize],
    /// Each arena's crate in the snapshot last built.
    crates: [Option<Crate>; ARENAS as usize],
    /// Whether a run homed at V, in the snapshot that owned V's arena, read R since V's arena
    /// was made: the borrow `Memo::next` must find, though V declared only the session.
    v_read_r: bool,
    /// Where V was carried while the R arena its input names was dropped: that arena. Every
    /// later use of the id is the named refusal.
    v_dead: Option<ArenaId>,
    /// The edit the next `advance` makes.
    edit: Option<Edit>,
    /// The pause of V's run `Memo::next` is to be called at, in this step.
    pause_at: Option<u32>,
    /// What `advance` made, when called at V's pause.
    made: Option<Result<Memo, String>>,
    seen: Seen,
}

/// The answers the snapshot asked must give: its values, crates and V's dead id.
#[derive(Clone, Copy)]
struct Now {
    values: [[u64; LEAVES]; ARENAS as usize],
    crates: [Option<Crate>; ARENAS as usize],
    v_dead: Option<ArenaId>,
}

impl Now {
    fn krate(&self, c: u32) -> Crate {
        self.crates[c as usize].expect("every arena's crate is made")
    }

    fn sum(&self, c: u32) -> u64 {
        self.values[c as usize].iter().fold(0_u64, |sum, n| sum.wrapping_add(*n))
    }

    /// The fresh evaluator of `total_of`.
    fn total(&self, c: u32) -> u64 {
        match c {
            R => self.sum(R),
            S => self.sum(S).wrapping_add(self.total(R).wrapping_mul(3)),
            U => self.sum(U).wrapping_add(self.total(S).wrapping_mul(3)),
            _ => unreachable!("total_of is asked of S and U"),
        }
    }

    /// The facts a query over the crates `of` reads: each one's crate input and its leaves.
    fn facts(&self, of: &[u32]) -> Vec<Fact, Global> {
        let mut out = Vec::new_in(Global);
        for c in of {
            out.push(crate_fact(*c));
            for (l, n) in self.values[*c as usize].iter().enumerate() {
                out.push(leaf_fact(*c, l, *n));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// The arenas `Memo::next` must drop for an edit of `edit`: it, and to a fixpoint every arena
/// that declared it (S of R, U of S) or read it (V of R, where `v_read_r`).
fn closure(edit: Option<u32>, v_read_r: bool) -> Vec<u32, Global> {
    let mut dropped = [false; ARENAS as usize];
    if let Some(edit) = edit {
        dropped[edit as usize] = true;
    }
    loop {
        let before = dropped;
        if dropped[R as usize] {
            dropped[S as usize] = true;
        }
        if dropped[S as usize] {
            dropped[U as usize] = true;
        }
        if dropped[R as usize] && v_read_r {
            dropped[V as usize] = true;
        }
        if dropped == before {
            break;
        }
    }
    let mut out = Vec::new_in(Global);
    out.extend((SESSION..ARENAS).filter(|c| dropped[*c as usize]));
    out
}

impl Driver {
    fn new(script: &Script, plant: Plant) -> Driver {
        Driver {
            plant,
            r_sealed: script.r_sealed,
            s_sealed: script.s_sealed,
            values: script.values,
            crates: [None; ARENAS as usize],
            v_read_r: false,
            v_dead: None,
            edit: None,
            pause_at: None,
            made: None,
            seen: Seen::default(),
        }
    }

    fn now(&self) -> Now {
        Now { values: self.values, crates: self.crates, v_dead: self.v_dead }
    }

    fn krate(&self, c: u32) -> Crate {
        self.crates[c as usize].expect("every arena's crate is made")
    }

    /// Adds arena `c` and its inputs to `builder`: its declared borrows, its seal, its leaves.
    fn make(&mut self, builder: &mut Builder, c: u32) {
        let session = builder.session();
        let mut deps: Vec<Crate, Global> = Vec::new_in(Global);
        let mut declared: Vec<ArenaId, Global> = Vec::new_in(Global);
        declared.push(session);
        match c {
            R => {}
            S => {
                deps.push(self.krate(R));
                declared.push(self.krate(R).gid().arena());
            }
            U => {
                deps.push(self.krate(S));
                declared.push(self.krate(S).gid().arena());
            }
            // V names R in its input and declares only the session.
            V => deps.push(self.krate(R)),
            _ => unreachable!("arenas 1 to 4"),
        }
        let values = self.values[c as usize];
        let inputs = builder.arena(c, Tag::Query);
        for from in declared {
            inputs.borrows(from);
        }
        if c == R {
            inputs.open();
        }
        if (c == R && self.r_sealed) || (c == S && self.s_sealed) {
            inputs.seal();
        }
        let mut leaves: Vec<Leaf, Global> = Vec::new_in(Global);
        for (l, n) in values.iter().enumerate() {
            leaves.push(Leaf::new(&inputs, leaf_fact(c, l, *n), *n));
        }
        self.crates[c as usize] = Some(Crate::new(&inputs, crate_fact(c), leaves, deps));
    }

    fn config(&self) -> Config {
        Config {
            arenas: ARENAS,
            reads: Reads::Kept,
            flight: Arc::new(Alone::new()),
            cap_bytes: u64::MAX,
            plant: self.plant,
        }
    }

    fn first(&mut self) -> Memo {
        let mut builder = Memo::builder(self.config());
        builder.arena(SESSION, Tag::Query);
        for c in R..ARENAS {
            self.make(&mut builder, c);
        }
        builder.build()
    }

    /// The edit, `old.next`, the oracle's first check (the dropped set, before anything kept
    /// is read), and the rebuild of what was dropped.
    fn advance(&mut self, old: &Memo) -> Result<Memo, String> {
        let edit = self.edit.take();
        let mut changed: Vec<ArenaId, Global> = Vec::new_in(Global);
        if let Some(edit) = edit {
            self.values[edit.arena as usize][edit.leaf] += edit.by;
            changed.push(self.krate(edit.arena).gid().arena());
        }
        let named = edit.map(|it| it.arena);
        let want = closure(named, self.v_read_r);
        let next = old.next(&changed).map_err(|why| format!("Memo::next refused: {why}"))?;
        let got = next.dropped();
        if got != want.as_slice() {
            let what = named.map_or("nothing", |it| NAMES[it as usize]);
            let mut why = format!(
                "Memo::next dropped {got:?}, but the closure of an edit of {what} over the \
                 declared borrows and V's reads (V read R: {}) is {want:?}",
                self.v_read_r
            );
            for c in want.iter().copied().filter(|c| !got.contains(c)) {
                let _ = write!(why, "; carried {}, a borrower of a dropped arena", NAMES[c as usize]);
            }
            for c in got.iter().copied().filter(|c| !want.contains(c)) {
                let _ = write!(why, "; dropped {}, which borrowed nothing dropped", NAMES[c as usize]);
            }
            return Err(why);
        }
        if (named == Some(R) && self.r_sealed) || (named == Some(S) && self.s_sealed) {
            self.seen.sealed_named += 1;
        }
        if named == Some(R) && self.v_read_r && got.contains(&V) {
            self.seen.dropped_by_read += 1;
        }
        let r_was = self.krate(R).gid().arena();
        let v_dropped = got.contains(&V);
        let r_dropped = got.contains(&R);
        let mut builder = next.builder;
        for c in R..ARENAS {
            if !builder.has(c) {
                self.make(&mut builder, c);
            }
        }
        if v_dropped {
            // Made again, its input naming R as it is now; its borrows are new.
            self.v_read_r = false;
            self.v_dead = None;
        } else if r_dropped && self.v_dead.is_none() {
            // Carried with its input naming the R arena just dropped.
            self.v_dead = Some(r_was);
        }
        Ok(builder.build())
    }
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
        .unwrap_or_default()
}

/// What an ask came to, for the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// S's or U's answer.
    Plain,
    /// V answered, having read R.
    ReadR,
    /// V refused its dead id of R by name.
    Refused,
}

/// V's view, asked plainly or under a recorder.
fn view_of<'a>(db: &'a SealDb<'_>, v: Crate, key: u32, recorded: bool) -> Result<(View<'a>, Option<Vec<Read, Global>>), String> {
    if !recorded {
        return Ok((view(db, v, key), None));
    }
    let (got, rows) = record(db.memo, || view(db, v, key)).map_err(|it| it.to_string())?;
    Ok((got, Some(rows)))
}

/// Asks `ask` of `db`'s snapshot and checks it against `now`: V's dead id refused by name; else
/// every interned id V kept live (a checked lookup, before anything V kept is read), every
/// answer the fresh evaluator's, every recorded list the evaluator's facts.
fn ask(db: &SealDb<'_>, now: &Now, ask: Ask) -> Result<Answer, String> {
    // The crate asked, and the crates whose facts its answer reads.
    let (c, of): (u32, &[u32]) = match ask.who {
        Who::S => (S, &[S, R] as &[u32]),
        Who::U => (U, &[U, S, R] as &[u32]),
        Who::V(_) => (V, &[V, R] as &[u32]),
    };
    let Who::V(key) = ask.who else {
        let krate = now.krate(c);
        let got = if ask.record {
            let (got, rows) = record(db.memo, || total_of(db, krate)).map_err(|it| it.to_string())?;
            let facts = facts_of(&rows);
            if facts.as_slice() != now.facts(of).as_slice() {
                return Err(format!("{ask:?}: the recorder listed {facts:?}, the evaluator reads {:?}", now.facts(of)));
            }
            got
        } else {
            total_of(db, krate)
        };
        if got != now.total(c) {
            return Err(format!("{ask:?} answered {got}, the evaluator {}", now.total(c)));
        }
        return Ok(Answer::Plain);
    };
    let v = now.krate(V);
    if let Some(dead) = now.v_dead {
        // V was carried holding an id of an R arena since dropped: its use is the named
        // refusal, never another making's rows.
        let tried = catch_unwind(AssertUnwindSafe(|| view_of(db, v, key, ask.record).map(|_| ())));
        return match tried {
            Ok(Ok(())) => Err(format!(
                "{ask:?}: V holds an id of {dead:?}, which was dropped, and was answered: the id \
                 read another making's rows"
            )),
            Ok(Err(why)) => Err(format!("{ask:?}: {why}")),
            Err(payload) => {
                let why = panic_text(payload);
                if why.contains(&format!("{dead:?} is dead")) {
                    Ok(Answer::Refused)
                } else {
                    Err(format!("{ask:?}: V holds an id of {dead:?}, dropped; the refusal does not name it: {why}"))
                }
            }
        };
    }
    let (got, rows) = view_of(db, v, key, ask.record)?;
    // The interned id first: a checked lookup that reads no arena it does not hold.
    let gid = got.tag.gid();
    let Some(tag) = RTag::try_from_gid(db, gid) else {
        return Err(format!("{ask:?}: the interned {gid:?} V kept names no live entry of the snapshot"));
    };
    let r = now.krate(R);
    let leaf = r.leaves(db)[0];
    let (want_n, got_n) = (now.values[R as usize][0], *tag.n(db));
    if tag != got.tag || *tag.leaf(db) != leaf || got_n != want_n {
        return Err(format!(
            "{ask:?}: the interned {gid:?} V kept holds n = {got_n}, not R's first value {want_n}, \
             or another leaf than R's first"
        ));
    }
    if *got.values != now.values[R as usize] || got.own != now.sum(V) || got.key != key {
        return Err(format!(
            "{ask:?} answered {:?} (own {}, key {}), the evaluator {:?} (own {}, key {key})",
            got.values,
            got.own,
            got.key,
            now.values[R as usize],
            now.sum(V)
        ));
    }
    if let Some(rows) = rows {
        let facts = facts_of(&rows);
        if facts.as_slice() != now.facts(of).as_slice() {
            return Err(format!("{ask:?}: the recorder listed {facts:?}, the evaluator reads {:?}", now.facts(of)));
        }
    }
    Ok(Answer::ReadR)
}

/// Asks `asks` of `memo`, the snapshot that owns every arena it holds, and keeps the model.
fn asks(memo: &Memo, pause: &Lock<Option<Pause>>, driver: &Lock<Driver>, asks: &[Ask]) -> Result<(), String> {
    for it in asks {
        let now = driver.lock().now();
        let db = SealDb { memo, pause, driver };
        match ask(&db, &now, *it)? {
            Answer::Plain => {}
            Answer::ReadR => driver.lock().v_read_r = true,
            Answer::Refused => driver.lock().seen.refused += 1,
        }
    }
    Ok(())
}

/// Runs `script` under `plant`: what it got to, or the first thing the oracle refused.
fn run_script(script: &Script, plant: Plant) -> Result<Seen, String> {
    let pause = Lock::new("blaze-memo sealed sim pause", None);
    let driver = Lock::new("blaze-memo sealed sim driver", Driver::new(script, plant));
    let mut current = driver.lock().first();
    for (at, step) in script.steps.iter().enumerate() {
        asks(&current, &pause, &driver, &step.asks)?;
        {
            let mut driver = driver.lock();
            driver.edit = step.edit.map(|arena| Edit { arena, leaf: at % LEAVES, by: 1 + at as u64 });
            driver.pause_at = step.paused;
        }
        let next = match step.paused {
            None => driver.lock().advance(&current)?,
            Some(pause_at) => {
                let now = driver.lock().now();
                *pause.lock() = Some(Pause { at: pause_at, seen: 0 });
                let paused = Ask { who: Who::V(PAUSED + at as u32), record: false };
                let answered = {
                    let db = SealDb { memo: &current, pause: &pause, driver: &driver };
                    ask(&db, &now, paused)
                };
                let unfired = pause.lock().take();
                let made = driver.lock().made.take();
                match (unfired, made) {
                    (None, Some(made)) => {
                        // The dropped set first; then the paused run's answer, which read the
                        // old snapshot's arenas, all alive.
                        let next = made?;
                        driver.lock().seen.paused += 1;
                        if answered? == Answer::Refused {
                            driver.lock().seen.refused += 1;
                        }
                        next
                    }
                    (Some(_), None) => {
                        // The run ended before the pause: only a refusal of V's dead id does.
                        if answered? != Answer::Refused {
                            return Err(format!("step {at}: V's run never reached pause {pause_at}"));
                        }
                        driver.lock().seen.refused += 1;
                        driver.lock().advance(&current)?
                    }
                    (unfired, made) => {
                        return Err(format!(
                            "step {at}: the pause fired ({}) but made {}",
                            unfired.is_none(),
                            if made.is_some() { "a snapshot" } else { "none" }
                        ));
                    }
                }
            }
        };
        driver.lock().pause_at = None;
        // The old snapshot released: every arena it alone held is freed.
        drop(current);
        current = next;
        asks(&current, &pause, &driver, &step.again)?;
    }
    let seen = driver.lock().seen;
    Ok(seen)
}

// The draws.

fn draw_ask(tape: &Tape) -> Ask {
    let who = match tape.below(3, "asked") {
        0 => Who::V(tape.below(2, "V's key") as u32),
        1 => Who::S,
        _ => Who::U,
    };
    Ask { who, record: tape.coin("recorded") }
}

fn draw(tape: &Tape, sealing: Sealing) -> Script {
    let (r_sealed, s_sealed) = match sealing {
        Sealing::Drawn => {
            let r = tape.coin("R sealed");
            (r, r && tape.coin("S sealed"))
        }
        Sealing::Sealed => (true, true),
        Sealing::Unsealed => (false, false),
    };
    let mut values = [[0; LEAVES]; ARENAS as usize];
    for row in values.iter_mut().skip(R as usize) {
        for n in row.iter_mut() {
            *n = tape.below(8, "leaf value");
        }
    }
    let mut steps = Vec::new_in(Global);
    steps.extend(tape.vec(1..=STEPS, "steps", |tape| {
        let mut asks = Vec::new_in(Global);
        asks.extend(tape.vec(0..=4, "asks", draw_ask));
        let paused = tape.one_in(3, "next while V runs").then(|| 1 + tape.below(2, "at V's pause") as u32);
        let edit = tape.pick(&EDITS, "edit").copied().flatten();
        let mut again = Vec::new_in(Global);
        again.extend(tape.vec(0..=4, "asks again", draw_ask));
        Step { asks, paused, edit, again }
    }));
    Script { r_sealed, s_sealed, values, steps }
}

fn run(tape: &Tape, plant: Plant, sealing: Sealing) -> Result<(), String> {
    let seen = run_script(&draw(tape, sealing), plant)?;
    if seen.sealed_named > 0 {
        tape.reached("a sealed arena named changed");
    }
    if seen.dropped_by_read > 0 {
        tape.reached("V dropped with R by its read alone");
    }
    if seen.paused > 0 {
        tape.reached("next while V's run is paused");
    }
    if seen.refused > 0 {
        tape.reached("a dead id refused by name");
    }
    Ok(())
}

// The tests.

/// The invariant: whatever the seals, the edits and when `Memo::next` is called (between asks,
/// or while V's run is paused on the old snapshot before or after it reads R), the arenas
/// `next` drops are exactly the closure of the edited one over the declared borrows (S of R, U
/// of S) and the reads made in the snapshot that owned the reader (V of R, though V declared
/// only the session): a sealed arena goes as any other, with every arena that borrows it. Then,
/// with the old snapshot dropped and its arenas freed, every answer is the fresh evaluator's,
/// every interned id V kept passes `try_from_gid`, every recorded list is the evaluator's
/// facts, and a carried V's id of a dropped R is refused by name. The dropped set is checked
/// before anything kept is read, so a missed borrow fails here and is never a read of freed
/// memory.
#[test]
fn a_sealed_arena_drops_with_every_arena_that_declared_or_read_it() {
    let config = blaze_sim::Config::cases(128).must_reach(&[
        "a sealed arena named changed",
        "V dropped with R by its read alone",
        "next while V's run is paused",
        "a dead id refused by name",
    ]);
    blaze_sim::check(config, |tape| run(tape, Plant::None, Sealing::Drawn));
}

/// The control: `Plant::SealedSkipsBorrow` (no borrow of a sealed arena noted, the shortcut
/// "a sealed arena never goes away") carries V past an edit of the sealed R it read, and the
/// oracle names it at the dropped set.
#[test]
fn the_oracle_catches_a_sealed_arena_skipping_its_borrows() {
    blaze_sim::control(
        blaze_sim::Config::cases(128),
        Plant::None,
        Plant::SealedSkipsBorrow,
        "carried V, a borrower of a dropped arena",
        |tape, plant| run(tape, plant, Sealing::Drawn).map_err(Broke::Oracle),
    );
}

/// The control of the control (the cause removed): with nothing sealed the plant has no borrow
/// to skip, and every case passes with it on.
#[test]
fn an_unsealed_world_passes_with_the_sealed_plant_on() {
    blaze_sim::check(blaze_sim::Config::cases(64), |tape| {
        run(tape, Plant::SealedSkipsBorrow, Sealing::Unsealed)
    });
}

/// The control: `Plant::IgnoreBorrows` carries V past an edit of the R it read, sealed or not.
#[test]
fn the_oracle_catches_ignored_borrows_of_a_sealed_arena() {
    blaze_sim::control(
        blaze_sim::Config::cases(128),
        Plant::None,
        Plant::IgnoreBorrows,
        "carried V, a borrower of a dropped arena",
        |tape, plant| run(tape, plant, Sealing::Drawn).map_err(Broke::Oracle),
    );
}

fn fixed_values() -> [[u64; LEAVES]; ARENAS as usize] {
    let mut values = [[0; LEAVES]; ARENAS as usize];
    for (c, row) in values.iter_mut().enumerate() {
        for (l, n) in row.iter_mut().enumerate() {
            *n = c as u64 * 10 + l as u64;
        }
    }
    values
}

fn one_step(sealing: Sealing, asks: &[Ask], paused: Option<u32>, again: &[Ask]) -> Script {
    let sealed = sealing == Sealing::Sealed;
    let mut step_asks = Vec::new_in(Global);
    step_asks.extend_from_slice(asks);
    let mut step_again = Vec::new_in(Global);
    step_again.extend_from_slice(again);
    let mut steps = Vec::new_in(Global);
    steps.push(Step { asks: step_asks, paused, edit: Some(R), again: step_again });
    Script { r_sealed: sealed, s_sealed: sealed, values: fixed_values(), steps }
}

const ASK_V: Ask = Ask { who: Who::V(0), record: true };
const ASK_S: Ask = Ask { who: Who::S, record: true };
const ASK_U: Ask = Ask { who: Who::U, record: false };

/// The case the plant must fail: V read the sealed R, R is edited, and V must not be carried;
/// then V, S and U are asked again of the next snapshot with the old one freed.
fn edit_r_after_v_read(plant: Plant, sealing: Sealing) -> Result<Seen, String> {
    run_script(&one_step(sealing, &[ASK_V, ASK_S, ASK_U], None, &[ASK_V, ASK_S, ASK_U]), plant)
}

/// The same with `Memo::next` called at V's pause `at` of its first run (1: before it read R,
/// so V is carried and its later ask is the named refusal; 2: after, so V is dropped).
fn edit_r_while_v_runs(plant: Plant, at: u32) -> Result<Seen, String> {
    run_script(&one_step(Sealing::Sealed, &[], Some(at), &[ASK_V, ASK_S, ASK_U]), plant)
}

/// The invariant, case by case: R sealed and edited after V read it drops V (its read alone
/// ties it to R) with S and U, and the next snapshot answers as the evaluator with the old
/// one freed; the same with `next` called while V's run is paused after its read; and before
/// its read, V is carried and its later use of R's dead id is refused by name. The planted
/// bugs: `SealedSkipsBorrow` and `IgnoreBorrows` carry V, caught at the dropped set. The
/// control: R unsealed, `SealedSkipsBorrow` has nothing to skip and the case passes.
#[test]
fn an_edit_of_a_sealed_r_drops_the_v_that_read_it() {
    let seen = edit_r_after_v_read(Plant::None, Sealing::Sealed).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!((seen.sealed_named, seen.dropped_by_read), (1, 1), "{seen:?}");
    for plant in [Plant::SealedSkipsBorrow, Plant::IgnoreBorrows] {
        let why = edit_r_after_v_read(plant, Sealing::Sealed).expect_err("V carried past its R's edit");
        assert!(why.contains("carried V, a borrower of a dropped arena"), "{plant:?}: {why}");
    }
    // The control: nothing sealed, nothing for the plant to skip.
    edit_r_after_v_read(Plant::SealedSkipsBorrow, Sealing::Unsealed).unwrap_or_else(|why| panic!("{why}"));

    // Paused after the read: the borrow was noted before the freeze.
    let seen = edit_r_while_v_runs(Plant::None, 2).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!((seen.paused, seen.dropped_by_read, seen.refused), (1, 1, 0), "{seen:?}");
    let why = edit_r_while_v_runs(Plant::SealedSkipsBorrow, 2).expect_err("V carried past its R's edit");
    assert!(why.contains("carried V, a borrower of a dropped arena"), "{why}");
    // Paused before the read: V read nothing of R in the snapshot that owns it, so it is
    // carried, its paused run's answer goes to the old snapshot's spill, and its later ask
    // meets the dead id.
    let seen = edit_r_while_v_runs(Plant::None, 1).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!((seen.paused, seen.dropped_by_read, seen.refused), (1, 0, 1), "{seen:?}");
}

/// The invariant: V holds R's crate id in its input (made with it; base-db hands out crate ids
/// with no read) and never read R; R, sealed, is edited and V carried. A later ask of V that
/// uses the id is refused by a panic naming the dead arena (`arena 1.0 is dead`), never
/// answered from the rows of R's next making. The control (the cause removed): V asked before
/// the edit read R, so it is dropped with R and the same later ask is answered.
#[test]
fn a_carried_v_holding_a_dead_sealed_id_is_refused_by_name() {
    let seen = run_script(&one_step(Sealing::Sealed, &[ASK_S], None, &[ASK_V, ASK_U]), Plant::None)
        .unwrap_or_else(|why| panic!("{why}"));
    assert_eq!((seen.dropped_by_read, seen.refused), (0, 1), "{seen:?}");
    let control = run_script(&one_step(Sealing::Sealed, &[ASK_V], None, &[ASK_V, ASK_U]), Plant::None)
        .unwrap_or_else(|why| panic!("{why}"));
    assert_eq!((control.dropped_by_read, control.refused), (1, 0), "{control:?}");
}
