//! The weak-memory world of threads that share atomic words, as a reference model: release and
//! acquire by views, where a load may read a stale value. Its model is `Blaze.Threads.Weak`
//! (`lean/Blaze/ThreadsWeak.lean`, docs/v2/plan/design/proofs-for-sans-io.md, section 8), and
//! this is that world, operation for operation; [`super::threads::two_weak`] runs two threads
//! against it, each operation of a scheduled atomic one step here.
//!
//! **Messages and views.** A cell is the list of the messages written to it, oldest first
//! ([`World::cells`], a cell's number its place); a message's place in that list is its
//! timestamp, and "newer" is later in the list. A [`Msg`] has a unique id (in order of
//! creation), a value, the [`View`] its writer published with it, and `glued`: an update read it
//! and wrote the message right after it in the same step, so nothing is ever put between the
//! two. A view names, per cell, the id of the newest message of that cell seen; a cell it does
//! not name, it has seen the first message of. The join of two views ([`join_view`]) takes, per
//! cell, the one whose message is newer.
//!
//! **Threads.** Thread 0 is the program run alone (before a run's threads start and after they
//! end); threads 1 and 2 are the run's. A thread other than 0 with no view yet has thread 0's
//! ([`World::view_of`]: a spawn shows the child everything its parent saw). Before each
//! operation of thread 0 its view becomes the join of every thread's view, and the others'
//! views and [`World::loaded`] are dropped ([`World::rejoin`]: a join shows the parent
//! everything its children saw).
//!
//! **Orderings.** A read acquires under `Acquire`, `AcqRel` and `SeqCst` ([`acquires`]), else it
//! is relaxed; a write releases under `Release`, `AcqRel` and `SeqCst` ([`releases`]). `SeqCst`
//! is `AcqRel` here and no more: sound, and it loses what only `SeqCst` promises (one total
//! order of the `SeqCst` operations), so store buffering is shown even with `SeqCst`. An update
//! that writes reads and writes with its `success` ordering; one that writes nothing reads with
//! its `failure` ordering.
//!
//! **The pick** (the schedule's step's; 0 for thread 0) says which message a read reads,
//! counted back from the newest (0 the newest), never older than the thread's view of the cell
//! ([`back`]). For a store it says where the new message goes: after the message that many back
//! from the newest, never at or before the thread's view; where that message is glued, after
//! the first one from there on that is not ([`unglued`]; the newest never is).
//!
//! **Operations**, one function each, taking who, the pick and the operation's arguments:
//!
//! - [`World::make`]: a new cell of one message, which its maker's view names.
//! - [`World::load`]: reads a message by the pick; the thread's view of the cell becomes it; an
//!   acquiring load also joins the message's view into the thread's; a relaxed one is recorded
//!   in [`World::loaded`].
//! - [`World::store`]: puts a new message where the pick says; the thread's view of the cell
//!   becomes it. The message's view is the thread's whole view where the store releases, else
//!   only itself.
//! - [`World::update`] (strong): pick k reads k back as a load does. Weak: pick 2k reads k back,
//!   pick 2k+1 fails for nothing having read k back ([`halve`]; an LL/SC store-exclusive fails
//!   whenever its load read a stale value, and returns that value). Where `change` gives a new
//!   value and the message read is glued, it reads the first one from there on that is not,
//!   and decides again. Where it writes, the new message goes right after the one read, which
//!   becomes glued; the views change as a load with `success`, then a store with `success`;
//!   and the new message's view is also joined with the view of the message read (C++20's
//!   release sequence). Where it writes nothing, it is a load of the message with `failure`.
//! - [`World::wait`] and [`World::asleep`] read the cell as a load does, by the pick, but never
//!   older than the newer of the thread's view and [`World::futex`]'s entry for the cell
//!   ([`World::look`]); the thread's view of the cell becomes the message read. A wait that
//!   returns woken joins `futex`'s entry for the cell into the thread's view. Asleep or woken,
//!   the pick means what it means in the sequentially consistent world: 1 returns for nothing,
//!   2 says a timed wait's time ran out. A run's end asks [`World::asleep`] with pick 0.
//! - [`World::wait_until`] and [`World::asleep_until`] are `wait` and `asleep` with the
//!   sequentially consistent world's deadline rules: a sleeper whose deadline the clock has
//!   reached returns timed out whatever the pick; pick 2 tells a sleeper with a deadline its
//!   time ran out and moves the clock to the deadline; a thread neither woken nor asleep reads
//!   by the pick and, where the message holds `expected` and the deadline has passed, returns
//!   timed out at once.
//! - [`World::clock`] and [`World::now`] are the sequentially consistent world's: the clock is
//!   not a cell. It is whole nanoseconds, a `u128`.
//! - [`World::wake`] makes `futex`'s entry for the cell the newer of itself and the waker's
//!   view of the cell, then wakes as the sequentially consistent world does: the sleeper of
//!   number `pick` among the cell's, the first where there is none of that number.
//!
//! **Refusals** ([`Refusal`], Lean's `none`). Rule R1: a write that does not release by a
//! thread other than 0 to a cell `y` is refused when that thread has done a relaxed read of a
//! cell other than `y`. The world has no promises, so it cannot show load buffering, which ARM
//! allows of plain loads and stores; a run that could show it is refused, not approximated.
//! Thread 0 is exempt: alone, nothing can tell. A cell number that is not a cell is refused.
//! A refused operation leaves the world as it was, but for thread 0's rejoin.
//!
//! Values are `u64` (a `bool` is 0 or 1), message ids `u64`, threads `u8`.

#![blaze::sans_io]

use std::alloc::Global;

use super::threads::Woke;
use crate::atomic::Ordering;

/// A thread: 0 the program alone, 1 and 2 a run's.
pub type Who = u8;

/// Per cell number, the id of the newest message of that cell seen. A cell not listed: its
/// first message.
pub type View = Vec<(usize, u64), Global>;

/// One write to a cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    /// Unique, in order of creation.
    pub id: u64,
    pub value: u64,
    /// What its writer published with it.
    pub view: View,
    /// An update wrote the message right after this one in the step it read this one.
    pub glued: bool,
}

/// What an atomic operation answers: a value, and a flag whose meaning is the operation's (an
/// update: whether it wrote). Lean's `Got`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Got {
    pub value: u64,
    pub flag: bool,
}

/// What a wait's step answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// It returned, for this reason.
    Returned(Woke),
    /// The thread sleeps in it still (or went to sleep in this step).
    Asleep,
}

/// Why the world refuses an operation: Lean's `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A cell number that is not a cell.
    NotACell,
    /// A place of a cell's list with no message: never, for a world made by its own rules.
    NoMessage,
    /// Rule R1: a write that does not release, after the thread's relaxed read of cell `read`.
    R1 { read: usize },
}

/// The value under `key` in an association list (Lean's `getAt`).
pub fn get_at<'a, K: PartialEq, V>(list: &'a [(K, V)], key: &K) -> Option<&'a V> {
    list.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// The association list with `key` set to `value`, appended where it was not there (Lean's
/// `putAt`).
pub fn put_at<K: PartialEq, V>(list: &mut Vec<(K, V), Global>, key: K, value: V) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) => entry.1 = value,
        None => list.push((key, value)),
    }
}

/// The place of the message `id` in `msgs` (Lean's `idxOf`).
pub fn idx_of(msgs: &[Msg], id: u64) -> Option<usize> {
    msgs.iter().position(|m| m.id == id)
}

/// The place a view's entry names in `msgs`: 0 where it names none (Lean's `placeIn`).
pub fn place_in(msgs: &[Msg], id: Option<u64>) -> usize {
    match id {
        Some(id) => idx_of(msgs, id).unwrap_or(0),
        None => 0,
    }
}

/// `view` joined with `other`: per cell, the newer message (Lean's `joinView`).
pub fn join_view(cells: &[Vec<Msg, Global>], view: &mut View, other: &[(usize, u64)]) {
    for &(cell, id) in other {
        let msgs: &[Msg] = cells.get(cell).map_or(&[], |msgs| msgs);
        let mine = place_in(msgs, get_at(view, &cell).copied());
        let theirs = idx_of(msgs, id).unwrap_or(0);
        if mine < theirs {
            put_at(view, cell, id);
        }
    }
}

/// The place `k` back from the newest of `len` messages, never before `floor`. The
/// subtractions stop at 0, as Lean's `Nat`'s do.
pub fn back(len: usize, k: usize, floor: usize) -> usize {
    let place = len.saturating_sub(1).saturating_sub(k);
    if place < floor { floor } else { place }
}

/// The first place from `place` on whose message is not glued (`msgs.len()` where every one
/// from there is; `place` where it is past the end).
pub fn unglued(msgs: &[Msg], place: usize) -> usize {
    let mut at = place;
    while at < msgs.len() && msgs[at].glued {
        at += 1;
    }
    at
}

/// `msgs` with `m` put at place `n` (at the end where `n` is past it). Answers where it went.
pub fn insert_at(msgs: &mut Vec<Msg, Global>, n: usize, m: Msg) -> usize {
    let at = n.min(msgs.len());
    msgs.insert(at, m);
    at
}

/// A weak update's pick `n` as `(k, odd)`, `n = 2k + (odd ? 1 : 0)`.
pub fn halve(n: usize) -> (usize, bool) {
    (n / 2, n % 2 == 1)
}

/// A read with `order` acquires.
pub fn acquires(order: Ordering) -> bool {
    matches!(order, Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst)
}

/// A write with `order` releases.
pub fn releases(order: Ordering) -> bool {
    matches!(order, Ordering::Release | Ordering::AcqRel | Ordering::SeqCst)
}

/// Whether the clock at `clock` has reached `deadline` (`None`: no deadline, never).
pub fn passed(clock: u128, deadline: Option<u128>) -> bool {
    match deadline {
        Some(deadline) => deadline <= clock,
        None => false,
    }
}

/// What threads share: messages per cell, each thread's view, and what rule R1, the futex and
/// the clock need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct World {
    /// Per cell, its messages oldest first; a cell's number is its place.
    pub cells: Vec<Vec<Msg, Global>, Global>,
    /// Thread to its view.
    pub views: Vec<(Who, View), Global>,
    /// The next message id.
    pub next: u64,
    /// (thread, cell) of every relaxed read (rule R1), newest first.
    pub loaded: Vec<(Who, usize), Global>,
    /// Per cell, the newest message any thread had seen of it when it called wake on it.
    pub futex: View,
    /// (thread, cell): asleep in a wait of the cell, newest first.
    pub asleep: Vec<(Who, usize), Global>,
    /// (thread, cell): woken by a wake, its wait not yet returned, newest first.
    pub woken: Vec<(Who, usize), Global>,
    /// The clock's last reading, in nanoseconds.
    pub clock: u128,
    /// The clock's script: the readings still to come, the next one last.
    pub readings: Vec<u128, Global>,
}

impl World {
    /// Nothing shared yet.
    pub fn new() -> World {
        World {
            cells: Vec::new_in(Global),
            views: Vec::new_in(Global),
            next: 0,
            loaded: Vec::new_in(Global),
            futex: Vec::new_in(Global),
            asleep: Vec::new_in(Global),
            woken: Vec::new_in(Global),
            clock: 0,
            readings: Vec::new_in(Global),
        }
    }

    /// Thread `who`'s view: thread 0's where it has none yet.
    pub fn view_of(&self, who: Who) -> &[(usize, u64)] {
        match get_at(&self.views, &who) {
            Some(view) => view,
            None => get_at(&self.views, &0).map_or(&[], |view| view),
        }
    }

    /// The value of `cell`'s newest message.
    pub fn newest(&self, cell: usize) -> Option<u64> {
        self.cells.get(cell).and_then(|msgs| msgs.last()).map(|m| m.value)
    }

    /// Thread 0's view becomes the join of every thread's; the others' and `loaded` are dropped.
    pub fn rejoin(&mut self) {
        let mut all = Vec::new_in(Global);
        for (_, view) in &self.views {
            join_view(&self.cells, &mut all, view);
        }
        self.views.clear();
        self.views.push((0, all));
        self.loaded.clear();
    }

    /// Before an operation of `who`: thread 0's rejoins.
    fn enter(&mut self, who: Who) {
        if who == 0 {
            self.rejoin();
        }
    }

    /// Rule R1: a write by `who` to `cell` with `order` is refused; `Some` of the other cell
    /// the thread read relaxed (the newest such read).
    pub fn refused(&self, who: Who, cell: usize, order: Ordering) -> Option<usize> {
        if who == 0 || releases(order) {
            return None;
        }
        self.loaded.iter().find(|(thread, read)| *thread == who && *read != cell).map(|(_, read)| *read)
    }

    /// `who` reads the message at `place` of `cell` with `order` (Lean's `readMsg`).
    fn read_msg(&mut self, who: Who, cell: usize, place: usize, order: Ordering) -> Result<(), Refusal> {
        let m = self.cells.get(cell).and_then(|msgs| msgs.get(place)).ok_or(Refusal::NoMessage)?;
        let mut view = self.view_of(who).to_vec_in(Global);
        put_at(&mut view, cell, m.id);
        if acquires(order) {
            join_view(&self.cells, &mut view, &m.view);
        }
        put_at(&mut self.views, who, view);
        if !acquires(order) && who != 0 && !self.loaded.contains(&(who, cell)) {
            self.loaded.insert(0, (who, cell));
        }
        Ok(())
    }

    /// `who` writes `value` to `cell` with `order`, at place `at` of the cell's messages as
    /// they are; the message's view is also joined with `extra` (Lean's `writeMsg`).
    fn write_msg(
        &mut self,
        who: Who,
        cell: usize,
        at: usize,
        value: u64,
        order: Ordering,
        extra: &[(usize, u64)],
    ) -> Result<(), Refusal> {
        let id = self.next;
        let mut view = self.view_of(who).to_vec_in(Global);
        put_at(&mut view, cell, id);
        let msgs = self.cells.get_mut(cell).ok_or(Refusal::NotACell)?;
        let place = insert_at(msgs, at, Msg { id, value, view: Vec::new_in(Global), glued: false });
        let mut published = if releases(order) {
            view.clone()
        } else {
            let mut only = Vec::with_capacity_in(1, Global);
            only.push((cell, id));
            only
        };
        join_view(&self.cells, &mut published, extra);
        let m = self.cells.get_mut(cell).and_then(|msgs| msgs.get_mut(place)).ok_or(Refusal::NoMessage)?;
        m.view = published;
        put_at(&mut self.views, who, view);
        self.next = id + 1;
        Ok(())
    }

    /// The place of the message a futex look by `who` of `cell` reads with `pick`: by the
    /// pick, never older than the newer of the thread's view and `futex`'s entry (Lean's
    /// `look`).
    pub fn look(&self, who: Who, cell: usize, pick: usize) -> Option<usize> {
        let msgs = self.cells.get(cell)?;
        let mine = place_in(msgs, get_at(self.view_of(who), &cell).copied());
        let woke = place_in(msgs, get_at(&self.futex, &cell).copied());
        let place = back(msgs.len(), pick, mine.max(woke));
        (place < msgs.len()).then_some(place)
    }

    /// `who`'s view of `cell` becomes the message at `place`; answers its value.
    fn sees(&mut self, who: Who, cell: usize, place: usize) -> Result<u64, Refusal> {
        let m = self.cells.get(cell).and_then(|msgs| msgs.get(place)).ok_or(Refusal::NoMessage)?;
        let (id, value) = (m.id, m.value);
        let mut view = self.view_of(who).to_vec_in(Global);
        put_at(&mut view, cell, id);
        put_at(&mut self.views, who, view);
        Ok(value)
    }

    /// A woken `who` returns from its wait of `cell`, the entry at `at` of `woken`: `futex`'s
    /// entry for the cell is joined into its view.
    fn woken_returns(&mut self, who: Who, cell: usize, at: usize) -> Waited {
        let mut view = self.view_of(who).to_vec_in(Global);
        if let Some(&id) = get_at(&self.futex, &cell) {
            join_view(&self.cells, &mut view, &[(cell, id)]);
        }
        self.woken.remove(at);
        put_at(&mut self.views, who, view);
        Waited::Returned(Woke::Woken)
    }

    /// A new cell holding `value`, made by `who`, whose view names its one message. Answers its
    /// number (Lean's `.new`).
    pub fn make(&mut self, who: Who, value: u64) -> usize {
        self.enter(who);
        let cell = self.cells.len();
        let id = self.next;
        self.cells.push(Self::first(cell, id, value));
        let mut view = self.view_of(who).to_vec_in(Global);
        put_at(&mut view, cell, id);
        put_at(&mut self.views, who, view);
        self.next = id + 1;
        cell
    }

    /// A cell holding `value` that thread 0 made before the run, seen by every thread: one
    /// message, which every view names (thread 0's made where it had none, so a thread with no
    /// view yet starts from it). Answers its number. Not a step: what a runner does where a
    /// run's thread first touches a word made outside the world.
    pub fn seen(&mut self, value: u64) -> usize {
        let cell = self.cells.len();
        let id = self.next;
        self.cells.push(Self::first(cell, id, value));
        if get_at(&self.views, &0).is_none() {
            self.views.push((0, Vec::new_in(Global)));
        }
        for (_, view) in self.views.iter_mut() {
            put_at(view, cell, id);
        }
        self.next = id + 1;
        cell
    }

    /// A cell's first message: `id`, holding `value`, whose view names itself.
    fn first(cell: usize, id: u64, value: u64) -> Vec<Msg, Global> {
        let mut view = Vec::with_capacity_in(1, Global);
        view.push((cell, id));
        let mut msgs = Vec::with_capacity_in(1, Global);
        msgs.push(Msg { id, value, view, glued: false });
        msgs
    }

    /// A load of `cell` with `order` by `who` with `pick`. Answers the value, flagged.
    pub fn load(&mut self, who: Who, pick: usize, cell: usize, order: Ordering) -> Result<Got, Refusal> {
        self.enter(who);
        let msgs = self.cells.get(cell).ok_or(Refusal::NotACell)?;
        let place = back(msgs.len(), pick, place_in(msgs, get_at(self.view_of(who), &cell).copied()));
        let value = msgs.get(place).ok_or(Refusal::NoMessage)?.value;
        self.read_msg(who, cell, place, order)?;
        Ok(Got { value, flag: true })
    }

    /// A store of `value` to `cell` with `order` by `who` with `pick`. Answers 0, flagged.
    pub fn store(&mut self, who: Who, pick: usize, cell: usize, value: u64, order: Ordering) -> Result<Got, Refusal> {
        self.enter(who);
        let msgs = self.cells.get(cell).ok_or(Refusal::NotACell)?;
        if let Some(read) = self.refused(who, cell, order) {
            return Err(Refusal::R1 { read });
        }
        let after = unglued(msgs, back(msgs.len(), pick, place_in(msgs, get_at(self.view_of(who), &cell).copied())));
        self.write_msg(who, cell, after + 1, value, order, &[])?;
        Ok(Got { value: 0, flag: true })
    }

    /// An update of `cell` by `who` with `pick`: reads, and where `change` of what it read is
    /// `Some(new)`, writes `new` right after it in the same step. `weak`: a
    /// `compare_exchange_weak`, whose odd picks fail for nothing. Answers what it read, flagged
    /// with whether it wrote.
    #[allow(clippy::too_many_arguments, reason = "Lean's `Op.update` and the step's who and pick, one argument each")]
    pub fn update(
        &mut self,
        who: Who,
        pick: usize,
        cell: usize,
        change: &dyn Fn(u64) -> Option<u64>,
        weak: bool,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Got, Refusal> {
        self.enter(who);
        let msgs = self.cells.get(cell).ok_or(Refusal::NotACell)?;
        let (k, spurious) = if weak { halve(pick) } else { (pick, false) };
        let read = back(msgs.len(), k, place_in(msgs, get_at(self.view_of(who), &cell).copied()));
        let m = msgs.get(read).ok_or(Refusal::NoMessage)?;
        if spurious || change(m.value).is_none() {
            return self.fails(who, cell, read, failure);
        }
        let read = unglued(msgs, read);
        let m = msgs.get(read).ok_or(Refusal::NoMessage)?;
        let Some(new) = change(m.value) else {
            return self.fails(who, cell, read, failure);
        };
        if let Some(other) = self.refused(who, cell, success) {
            return Err(Refusal::R1 { read: other });
        }
        let (value, extra) = (m.value, m.view.clone());
        self.read_msg(who, cell, read, success)?;
        let m = self.cells.get_mut(cell).and_then(|msgs| msgs.get_mut(read)).ok_or(Refusal::NoMessage)?;
        m.glued = true;
        self.write_msg(who, cell, read + 1, new, success, &extra)?;
        Ok(Got { value, flag: true })
    }

    /// An update that writes nothing: a load of the message at `place` with `failure`.
    fn fails(&mut self, who: Who, cell: usize, place: usize, failure: Ordering) -> Result<Got, Refusal> {
        let value = self.cells.get(cell).and_then(|msgs| msgs.get(place)).ok_or(Refusal::NoMessage)?.value;
        self.read_msg(who, cell, place, failure)?;
        Ok(Got { value, flag: false })
    }

    /// A futex wait of `cell` while it holds `expected` by `who` with `pick`; `timed`: the
    /// kernel may say its time ran out (pick 2).
    pub fn wait(&mut self, who: Who, pick: usize, cell: usize, expected: u64, timed: bool) -> Result<Waited, Refusal> {
        self.enter(who);
        if cell >= self.cells.len() {
            return Err(Refusal::NotACell);
        }
        if let Some(at) = self.woken.iter().position(|it| *it == (who, cell)) {
            return Ok(self.woken_returns(who, cell, at));
        }
        if let Some(at) = self.asleep.iter().position(|it| *it == (who, cell)) {
            if pick == 1 {
                self.asleep.remove(at);
                return Ok(Waited::Returned(Woke::Spurious));
            }
            if pick == 2 && timed {
                self.asleep.remove(at);
                return Ok(Waited::Returned(Woke::TimedOut));
            }
            return Ok(Waited::Asleep);
        }
        let place = self.look(who, cell, pick).ok_or(Refusal::NoMessage)?;
        if self.sees(who, cell, place)? != expected {
            return Ok(Waited::Returned(Woke::Changed));
        }
        self.asleep.insert(0, (who, cell));
        Ok(Waited::Asleep)
    }

    /// A futex wake of one sleeper of `cell` (`all`: of every one) by `who` with `pick`.
    /// Answers how many it woke.
    pub fn wake(&mut self, who: Who, pick: usize, cell: usize, all: bool) -> Result<usize, Refusal> {
        self.enter(who);
        if cell >= self.cells.len() {
            return Err(Refusal::NotACell);
        }
        if let Some(&id) = get_at(self.view_of(who), &cell) {
            join_view(&self.cells, &mut self.futex, &[(cell, id)]);
        }
        let mut on_cell = self.asleep.iter().enumerate().filter(|(_, it)| it.1 == cell).map(|(at, _)| at);
        let first = on_cell.clone().next();
        let Some(at) = on_cell.nth(pick).or(first) else {
            return Ok(0);
        };
        if all {
            let mut sleepers = Vec::new_in(Global);
            sleepers.extend(self.asleep.iter().copied().filter(|it| it.1 == cell));
            self.asleep.retain(|it| it.1 != cell);
            let woke = sleepers.len();
            self.woken.splice(0..0, sleepers);
            return Ok(woke);
        }
        let one = self.asleep.remove(at);
        self.woken.insert(0, one);
        Ok(1)
    }

    /// Not an operation of a program: whether `who`'s wait of `cell` for `expected` would not
    /// return, asked with `pick` (a run's end asks with 0).
    pub fn asleep(&mut self, who: Who, pick: usize, cell: usize, expected: u64) -> Result<bool, Refusal> {
        self.asleep_until(who, pick, cell, expected, None)
    }

    /// Installs the clock's script: the readings still to come, the next one last.
    pub fn clock(&mut self, who: Who, readings: Vec<u128, Global>) {
        self.enter(who);
        self.readings = readings;
    }

    /// Reads the clock. `moves`: it first becomes the later of itself and the script's next
    /// reading, which it takes (with the script empty it stays).
    pub fn now(&mut self, who: Who, moves: bool) -> u128 {
        self.enter(who);
        if moves && let Some(reading) = self.readings.pop() {
            self.clock = self.clock.max(reading);
        }
        self.clock
    }

    /// A futex wait with a deadline on the clock (`None`: none), by `who` with `pick`.
    pub fn wait_until(
        &mut self,
        who: Who,
        pick: usize,
        cell: usize,
        expected: u64,
        deadline: Option<u128>,
    ) -> Result<Waited, Refusal> {
        self.enter(who);
        if cell >= self.cells.len() {
            return Err(Refusal::NotACell);
        }
        if let Some(at) = self.woken.iter().position(|it| *it == (who, cell)) {
            return Ok(self.woken_returns(who, cell, at));
        }
        if let Some(at) = self.asleep.iter().position(|it| *it == (who, cell)) {
            if passed(self.clock, deadline) {
                self.asleep.remove(at);
                return Ok(Waited::Returned(Woke::TimedOut));
            }
            if pick == 1 {
                self.asleep.remove(at);
                return Ok(Waited::Returned(Woke::Spurious));
            }
            if let Some(deadline) = deadline
                && pick == 2
            {
                self.asleep.remove(at);
                self.clock = deadline;
                return Ok(Waited::Returned(Woke::TimedOut));
            }
            return Ok(Waited::Asleep);
        }
        let place = self.look(who, cell, pick).ok_or(Refusal::NoMessage)?;
        if self.sees(who, cell, place)? != expected {
            return Ok(Waited::Returned(Woke::Changed));
        }
        if passed(self.clock, deadline) {
            return Ok(Waited::Returned(Woke::TimedOut));
        }
        self.asleep.insert(0, (who, cell));
        Ok(Waited::Asleep)
    }

    /// Not an operation of a program: whether `who`'s `wait_until` of `cell` for `expected`
    /// would not return, asked with `pick` (a run's end asks with 0). Changes nothing but
    /// thread 0's rejoin.
    pub fn asleep_until(
        &mut self,
        who: Who,
        pick: usize,
        cell: usize,
        expected: u64,
        deadline: Option<u128>,
    ) -> Result<bool, Refusal> {
        self.enter(who);
        let msgs = self.cells.get(cell).ok_or(Refusal::NotACell)?;
        let place = self.look(who, cell, pick).ok_or(Refusal::NoMessage)?;
        let value = msgs.get(place).ok_or(Refusal::NoMessage)?.value;
        Ok(!self.woken.contains(&(who, cell))
            && (self.asleep.contains(&(who, cell)) || value == expected)
            && !passed(self.clock, deadline))
    }
}

impl Default for World {
    fn default() -> World {
        World::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAXED: Ordering = Ordering::Relaxed;
    const RELEASE: Ordering = Ordering::Release;
    const ACQUIRE: Ordering = Ordering::Acquire;

    fn value(got: Result<Got, Refusal>) -> u64 {
        match got {
            Ok(got) => got.value,
            Err(refusal) => panic!("the weak world refused a litmus step: {refusal:?}"),
        }
    }

    /// Store buffering (Tests/ThreadsWeak.lean:33), stepped by hand: thread 0 makes x and y,
    /// threads 1 and 2 each store one with `Release` and load the other with `Acquire`, picking
    /// the stale message: both read 0.
    #[test]
    fn store_buffering_stepped() {
        let mut w = World::new();
        let x = w.make(0, 0);
        let y = w.make(0, 0);
        value(w.store(1, 0, x, 1, RELEASE));
        value(w.store(2, 0, y, 1, RELEASE));
        assert_eq!(value(w.load(1, 1, y, ACQUIRE)), 0);
        assert_eq!(value(w.load(2, 1, x, ACQUIRE)), 0);
        // Pick 0 reads the newest, as the sequentially consistent world would.
        assert_eq!(value(w.load(1, 0, y, ACQUIRE)), 1);
    }

    /// Spawn and join (Tests/ThreadsWeak.lean:129..141), with thread 0's own steps: its store
    /// before the run floors thread 1's read whatever the pick, and its rejoin after shows it
    /// thread 2's store.
    #[test]
    fn spawn_and_join_stepped() {
        for pick in [0, 1, 2] {
            let mut w = World::new();
            let x = w.make(0, 0);
            let y = w.make(0, 0);
            value(w.store(0, 0, x, 5, RELAXED));
            assert_eq!(value(w.load(1, pick, x, RELAXED)), 5);
            value(w.store(2, 0, y, 9, RELAXED));
            // Thread 0 rejoins: the stale 0 of y is behind its view now, whatever it picks.
            assert_eq!(value(w.load(0, 1, y, RELAXED)), 9);
            assert!(w.loaded.is_empty(), "the rejoin drops `loaded`");
            assert_eq!(w.views.len(), 1, "the rejoin drops every view but thread 0's");
        }
    }

    /// Two `fetch_add`s (Tests/ThreadsWeak.lean:95): the second update's pick points back at
    /// the glued message, so it reads the newest, and the total is 2.
    #[test]
    fn an_update_skips_the_glued_message() {
        let add = |old: u64| Some(old + 1);
        let mut w = World::new();
        let n = w.make(0, 0);
        assert_eq!(w.update(2, 3, n, &add, false, RELAXED, RELAXED), Ok(Got { value: 0, flag: true }));
        assert_eq!(w.update(1, 7, n, &add, false, RELAXED, RELAXED), Ok(Got { value: 1, flag: true }));
        assert!(w.cells[n][0].glued && w.cells[n][1].glued && !w.cells[n][2].glued);
        assert_eq!(value(w.load(0, 0, n, RELAXED)), 2);
    }

    /// Rule R1 (Tests/ThreadsWeak.lean:121..123): a relaxed store after a relaxed load of
    /// another cell is refused, naming the cell read; a release store, or thread 0's, is not.
    #[test]
    fn rule_r1_stepped() {
        let mut w = World::new();
        let x = w.make(0, 0);
        let y = w.make(0, 0);
        value(w.load(1, 0, x, RELAXED));
        assert_eq!(w.store(1, 0, y, 1, RELAXED), Err(Refusal::R1 { read: x }));
        assert_eq!(w.update(1, 0, y, &|_| Some(1), false, RELAXED, RELAXED), Err(Refusal::R1 { read: x }));
        // The same cell is not refused, nor is a release, nor another thread.
        assert!(w.store(1, 0, x, 1, RELAXED).is_ok());
        assert!(w.store(1, 0, y, 1, RELEASE).is_ok());
        assert!(w.store(2, 0, y, 2, RELAXED).is_ok());
        assert_eq!(w.store(1, 0, 7, 1, RELEASE), Err(Refusal::NotACell));
    }

    /// The weak compare-and-swap (Tests/ThreadsWeak.lean:211): pick 5 = 2*2+1 fails for nothing
    /// having read 2 back; pick 4 reads it and writes right after it.
    #[test]
    fn a_weak_update_halves_its_pick() {
        assert_eq!((halve(4), halve(5), halve(0), halve(1)), ((2, false), (2, true), (0, false), (0, true)));
        let cas = |old: u64| (old == 0).then_some(5);
        let made = || {
            let mut w = World::new();
            let x = w.make(0, 0);
            value(w.store(2, 0, x, 1, RELAXED));
            value(w.store(2, 0, x, 0, RELAXED));
            (w, x)
        };
        let (mut w, x) = made();
        assert_eq!(w.update(1, 5, x, &cas, true, RELAXED, RELAXED), Ok(Got { value: 0, flag: false }));
        let (mut w2, x2) = made();
        assert_eq!(w2.update(1, 4, x2, &cas, true, RELAXED, RELAXED), Ok(Got { value: 0, flag: true }));
        assert!(w2.cells[x2].iter().map(|m| m.value).eq([0, 5, 1, 0]), "5 goes right after the first 0");
        assert_eq!(value(w.load(1, 1, x, RELAXED)), 1);
    }
}
