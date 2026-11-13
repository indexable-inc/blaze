//! Capturing what a piece of a query read, and replaying it into another query (rustc's
//! `with_cached_task` and `get_tracked`): one query reuses a result another computed (a solved
//! trait goal) and still reruns when what that result read changes.
//!
//! A capture frame belongs to the running query: every read in it is the query's, as it
//! happens. Its reads are the edge stack above where it opened, in read order (a read the query
//! made before is pushed again inside the frame), so a nested frame's reads and what a replay
//! pushed in it are the enclosing frame's too. Closing stores them as a [`Reads`], valid in the
//! view it was captured in, and freed with it: each read once, and each nested capture or replay
//! as one entry naming that capture, so the solver's goals, each captured inside its parent's,
//! hold every read once rather than once per level (`Storage::push_capture`).

use std::ops::ControlFlow;

use blaze_hash::FixedState;

use super::{
    Database, DatabaseKeyIndex, Id, Storage,
    function,
    ingredient::{self, Kind},
    local::{self, Capture, walk_capture},
    row::{Header, PROVISIONAL, RUNNING, STORED, VIEW_MASK, span},
    storage::{Summary, key_parts, or},
};

/// The bit of a captured entry that makes it a capture's number, not a node's.
pub(crate) const REF: u32 = 1 << 31;

/// What a [`capture_reads`] frame read: its edges, the lowest durability and highest
/// `changed` of them, the OR of their input summaries, and the view it was captured in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reads {
    /// Its capture's number in its view, 0 for no read.
    capture: u32,
    /// The distinct nodes it read.
    len: u32,
    view: u32,
    /// `changed`, with the durability in its top two bits.
    changed: u32,
    summary: Summary,
}

const _: () = assert!(size_of::<Reads>() == 32);

const DURABILITY_SHIFT: u32 = 30;

/// One read of a [`Reads`].
#[derive(Clone, Copy, Debug)]
pub struct Read {
    pub key: DatabaseKeyIndex,
    pub kind: ReadKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadKind {
    Function,
    /// The input field's number.
    InputField(u32),
    /// The tracked field's number among the `#[tracked]` ones.
    TrackedField(u32),
    /// An interned read records no edge: never reported.
    Interned,
}

impl Reads {
    /// The distinct dependencies it read.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn durability(self) -> u8 {
        (self.changed >> DURABILITY_SHIFT) as u8
    }

    fn changed(self) -> u32 {
        self.changed & ((1 << DURABILITY_SHIFT) - 1)
    }

    fn check_view(self, storage: &Storage) {
        assert_eq!(
            self.view,
            storage.view_bits(),
            "`Reads` captured in view {} used in view {}: keep shared results keyed by view",
            self.view,
            storage.view_bits()
        );
    }

    /// Calls `f` with each read, in read order, until it breaks. A read made in several
    /// captures nested in this one may come more than once.
    ///
    /// # Panics
    ///
    /// If `self` was captured in another view than `storage`'s.
    pub fn for_each_read<B>(
        &self,
        storage: &Storage,
        mut f: impl FnMut(Read) -> ControlFlow<B>,
    ) -> Option<B> {
        self.check_view(storage);
        // `f` may ask the database, so the walk's lists are its own, not the thread's.
        let mut spans = Vec::new(); // alloc: a walk's open captures, once per read-out of a capture (a store's offer)
        let mut walked = hashbrown::HashSet::with_hasher(FixedState); // alloc: as above
        walk_capture(storage, self.capture, &mut spans, &mut walked, |node| {
            f(read_of_key(storage.key_of(node)))
        })
    }
}

/// One read of [`Reads::for_each_read_through`].
#[derive(Clone, Copy, Debug)]
pub enum RowRead {
    /// A read: one the capture made, or one of a row it was told to descend through.
    Read(Read),
    /// A row it was told to descend through whose last run was answered from a store by a
    /// key ([`super::report_stored_read`]): its kept edges are that lookup's reads, not the
    /// reads its value was first made from, so it is handed over whole and not descended.
    /// The caller reads that record's own shape.
    Stored(Read),
    /// A row it was told to descend through that could not be brought up to date in this
    /// view ([`super::Refreshed::Unsettled`]: on this task's stack, provisional): its kept
    /// reads are not what its value rests on now, so it is handed over whole.
    Unsettled(Read),
}

/// The header of the function row `read` names, when it is current in this view: verified
/// in it, neither running nor provisional.
///
/// # Panics
///
/// When it is not, naming the row: its kept edges may be an older run's, or about to be
/// replaced, and a list of them would be taken for what its value rests on now.
fn current_row<'s>(storage: &'s Storage, read: &Read, why: &str) -> &'s Header {
    let (index, id) = (read.key.ingredient_index(), read.key.key_index());
    let current = function::header_of(storage, index, id).filter(|header| {
        let word = header.verified.load(std::sync::atomic::Ordering::Acquire);
        word & (RUNNING | PROVISIONAL) == 0 && word & VIEW_MASK == storage.view_bits()
    });
    match current {
        Some(header) => header,
        None => panic!(
            "the kept reads of `{}`({id:?}) were asked for ({why}) and the row is not up to \
             date in this view: refresh it first",
            ingredient::name(index)
        ),
    }
}

impl Reads {
    /// [`Reads::for_each_read`], but where `through` says so of a read of a function's row,
    /// that row's own kept reads are given in its place (and so on through them), the row
    /// itself not given: for a caller that wants what a value rests on below some rows it
    /// names (a body's lowering below `Body::of`). Each node is given once, whichever way it
    /// is reached.
    ///
    /// What holds of a row descended through:
    /// - its reads are those of its last run, kept until it runs again: letting its value go
    ///   (`evict`, an epoch's end, a budget's pass) keeps them;
    /// - it is brought up to date in this view first, by its key ([`super::refresh`]): a row
    ///   its reader proved by durability keeps an older view's stamp until it is asked for.
    ///   Once current it does not run again in the view, so its reads are not replaced under
    ///   the walk, and the edge array moves only under `&mut`. The running query, if any,
    ///   reads each row descended (the refresh records it), so it runs again when one of
    ///   them does and its kept reads change;
    /// - one that cannot be brought up to date is handed over as [`RowRead::Unsettled`], and
    ///   one answered from a store as [`RowRead::Stored`]: neither is descended.
    ///
    /// An interned read records no edge, so none is given, here or in a row descended.
    ///
    /// # Panics
    ///
    /// If `self` was captured in another view than `db`'s storage's, or as
    /// [`super::refresh`] does.
    pub fn for_each_read_through<D: Database, B>(
        &self,
        db: &D,
        through: impl Fn(&Read) -> bool,
        mut each: impl FnMut(RowRead) -> ControlFlow<B>,
    ) -> Option<B> {
        let storage = db.storage();
        self.check_view(storage);
        // `each` may ask the database, so the walk's lists are its own, not the thread's.
        let mut spans = Vec::new(); // alloc: a walk's open captures, once per read-out of a capture
        let mut walked = hashbrown::HashSet::with_hasher(FixedState); // alloc: as above
        let mut seen: hashbrown::HashSet<u32, FixedState> = hashbrown::HashSet::with_hasher(FixedState); // alloc: as above
        let mut rows: Vec<(u64, u64)> = Vec::new(); // alloc: the rows being descended, a few
        walk_capture(storage, self.capture, &mut spans, &mut walked, |node| {
            // Depth first, in read order: the node itself, then (for a row descended through)
            // its edges, each of which may be descended in turn. Bounded: a node is taken
            // once (`seen`).
            let mut next = Some(node);
            loop {
                let node = match next.take() {
                    Some(node) => node,
                    None => {
                        let Some(top) = rows.last_mut() else { return ControlFlow::Continue(()) };
                        if top.0 == top.1 {
                            rows.pop();
                            continue;
                        }
                        top.0 += 1;
                        storage.edge(top.0 - 1)
                    }
                };
                if !seen.insert(node) {
                    continue;
                }
                let read = read_of_key(storage.key_of(node));
                if read.kind != ReadKind::Function || !through(&read) {
                    if let ControlFlow::Break(out) = each(RowRead::Read(read)) {
                        rows.clear();
                        return ControlFlow::Break(out);
                    }
                    continue;
                }
                if super::refresh(db, read.key) != super::Refreshed::Current {
                    if let ControlFlow::Break(out) = each(RowRead::Unsettled(read)) {
                        rows.clear();
                        return ControlFlow::Break(out);
                    }
                    continue;
                }
                let header = current_row(storage, &read, "to descend through it");
                if header.meta.load(std::sync::atomic::Ordering::Relaxed) & STORED != 0 {
                    if let ControlFlow::Break(out) = each(RowRead::Stored(read)) {
                        rows.clear();
                        return ControlFlow::Break(out);
                    }
                    continue;
                }
                let kept = header.edges.load(std::sync::atomic::Ordering::Acquire);
                rows.push((span::start(kept), span::start(kept) + span::len(kept))); // alloc: as above
            }
        })
    }
}

impl Storage {
    /// The kept reads of the function row `row`, in read order: those of its last run.
    /// [`Reads::for_each_read_through`] says what holds of them.
    ///
    /// # Panics
    ///
    /// When the row is not current in this view, naming it: bring it up to date first
    /// ([`super::refresh`]).
    pub fn row_reads<B>(
        &self,
        row: DatabaseKeyIndex,
        mut each: impl FnMut(Read) -> ControlFlow<B>,
    ) -> Option<B> {
        let read = Read { key: row, kind: ReadKind::Function };
        let header = current_row(self, &read, "its own");
        let kept = header.edges.load(std::sync::atomic::Ordering::Acquire);
        for edge in span::start(kept)..span::start(kept) + span::len(kept) {
            if let ControlFlow::Break(out) = each(read_of_key(self.key_of(self.edge(edge)))) {
                return Some(out);
            }
        }
        None
    }
}

/// The read a node's key word names.
pub(crate) fn read_of_key(key: u64) -> Read {
    let (index, id, field) = key_parts(key);
    let kind = match ingredient::kind(index) {
        Some(Kind::Input) => ReadKind::InputField(field),
        Some(Kind::Tracked) => ReadKind::TrackedField(field),
        Some(Kind::Interned) => ReadKind::Interned,
        Some(Kind::Function) | None => ReadKind::Function,
    };
    Read { key: DatabaseKeyIndex::new(index, id), kind }
}

/// Drops an unclosed capture (an unwind out of its `f`): its reads stay the query's, but the
/// frames around it may not be shared.
struct Open {
    at: usize,
    closed: bool,
}

impl Drop for Open {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        local::with(|local| {
            local.captures.truncate(self.at + 1);
            if local.captures.len() == self.at + 1
                && let Some(capture) = local.captures.pop()
                && let Some(frame) = local.frames.get_mut(capture.frame as usize)
            {
                frame.durability = frame.durability.min(capture.durability);
                frame.changed = frame.changed.max(capture.changed);
                frame.summary = or(frame.summary, capture.summary);
                frame.unshareable = true;
                frame.dedup_floor = capture.dedup_floor;
            }
        });
    }
}

/// Runs `f` in a capture frame of the running query and returns what it read; `None` when
/// what `f` computed must not be shared with another query: it read a provisional value, made
/// a tracked struct or an untracked read, or called [`unshareable_reads`] (here or in a frame
/// nested in this one), or ran with no running query.
pub fn capture_reads<D: Database, T>(db: &D, f: impl FnOnce() -> T) -> (T, Option<Reads>) {
    let storage = db.storage();
    let opened = local::with(|local| {
        let edges = local.edges.len() as u32;
        let frame_at = local.frames.len().checked_sub(1)? as u32;
        let frame = local.recording(storage)?;
        let capture = Capture {
            frame: frame_at,
            edges_start: edges,
            durability: frame.durability,
            changed: frame.changed,
            summary: frame.summary,
            unshareable: frame.unshareable,
            dedup_floor: frame.dedup_floor,
        };
        frame.durability = u8::MAX;
        frame.changed = 0;
        frame.summary = [0; 2];
        frame.unshareable = false;
        frame.dedup_floor = edges;
        // alloc: none once warm: the capture stack grows to its high-water depth once per thread
        local.captures.push(capture);
        Some(local.captures.len() - 1)
    });
    let Some(at) = opened else { return (f(), None) };
    let mut open = Open { at, closed: false };
    let value = f();
    open.closed = true;
    let reads = local::with(|local| {
        let capture = local.captures.pop().expect("the capture `capture_reads` opened");
        debug_assert_eq!(local.captures.len(), at);
        let frame = &mut local.frames[capture.frame as usize];
        let (durability, changed, replayed, unshareable) =
            (frame.durability, frame.changed, frame.summary, frame.unshareable);
        frame.durability = durability.min(capture.durability);
        frame.changed = changed.max(capture.changed);
        frame.summary = or(replayed, capture.summary);
        frame.unshareable = unshareable || capture.unshareable;
        frame.dedup_floor = capture.dedup_floor;
        let owner = (frame.ingredient, Id::from_u32(frame.id));
        if unshareable {
            return None;
        }
        let start = capture.edges_start as usize;
        let number = storage.push_capture(local.capture_entries(start), owner);
        // Its run's count stops rising here: the dedup below lowers it.
        let (_, note) = local.note_peak(capture.frame as usize);
        let before = local.edges.len();
        local.dedup_from(start);
        local.frames[capture.frame as usize].dropped += (before - local.edges.len()) as u64;
        local.mark_capture(start, number);
        let edges = &local.edges[start..];
        let durability = u32::from(durability.min(2));
        Some((
            Reads {
                capture: number,
                len: edges.len() as u32,
                view: storage.view_bits(),
                changed: changed | (durability << DURABILITY_SHIFT),
                summary: or(replayed, storage.summary_of_edges(edges)),
            },
            note,
        ))
    });
    let reads = reads.map(|(reads, note)| {
        // The run is on top again: a new mark names it.
        super::noted(storage, note);
        reads
    });
    (value, reads)
}

/// Makes the running query depend on `reads` as if it had made them. Inside a capture frame
/// they are the frame's reads too. With no running query it does nothing.
///
/// # Panics
///
/// If `reads` was captured in another view.
pub fn replay_reads<D: Database>(db: &D, reads: Reads) {
    let storage = db.storage();
    reads.check_view(storage);
    let note = local::with(|local| {
        let Some(frame) = local.recording(storage) else { return local::Note::Fine };
        frame.durability = frame.durability.min(reads.durability());
        frame.changed = frame.changed.max(reads.changed());
        frame.summary = or(frame.summary, reads.summary);
        local.replay(storage, reads.capture)
    });
    super::noted(storage, note);
}

/// Appends to `out` the tracing text `write` writes: a span's name (`hir_def::span_names`).
///
/// Reads here are not tracked; the result must never reach a query's value. The running query
/// records none of `write`'s reads, so its edges are the same whether or not a profiler records
/// its span; text that reached its value would go stale with nothing to invalidate it. So only
/// text leaves: `write` returns nothing and writes only into `out`. `.clippy.toml` refuses every
/// caller but `span_names`. A query `write` asks for still runs, and records its reads into its
/// own row.
///
/// A query `write` asks for that is on this thread's stack joins the running query to that
/// cycle as an ask outside the guard would ([`unrecorded`]).
pub fn for_span_text<D: Database>(db: &D, out: &mut String, write: impl FnOnce(&D, &mut String)) {
    let _restore = Unrecorded::open(db);
    write(db, out);
}

/// Runs `f` with none of its reads recorded into `db`'s running query or any capture open on
/// it ([`capture_reads`]): when `f` returns or unwinds, the run's frame is as `f` found it.
/// For bookkeeping about what the query computed whose result never reaches its value: a
/// goal's offer to the goal store, which hashes the facts the goal read into the store's own
/// record (`hir_ty::next_solver::goal_store`), so the trace of the body whose solve first
/// offered a goal is the same as one that took it from the goal table. `.clippy.toml` limits
/// its callers to those.
///
/// What it restores, exactly: the edge stack's length (its segments too), and what the reads
/// folded into the frame (the lowest durability, the highest `changed`, the replayed
/// summaries, whether it may be shared, whether it made an untracked or a stored read, its
/// dedup count and limit). While `f` runs the frame's dedup floor is the stack's top, as a
/// capture's is, so a dedup inside `f` moves none of the edges from before it. A capture `f`
/// opens and closes is kept whole (its reads are its own, stored with it) and gone from the
/// frame with the rest.
///
/// Sound under the engine's cycles: a cycle `f` meets through a row on the stack at or below
/// the running query stays the run's (its cycle head, and whether a row of the stack heads a
/// cycle or did not converge, are never put back), so the run is that cycle's member, run
/// again with it, as an ask outside the guard would make it; only that membership is kept, as
/// what `f` read still reaches no value. Under parallel loops: a task `f` runs on another
/// worker records into no frame of this one, and one it runs here records above where `f`
/// began. Nothing is held across a wait but stack indices.
pub fn unrecorded<D: Database, T>(db: &D, f: impl FnOnce() -> T) -> T {
    let _restore = Unrecorded::open(db);
    f()
}

/// [`unrecorded`], unless `keep` says of `f`'s value that its reads are the running query's
/// after all: then they stay recorded, and in every capture open on it, as if no guard had
/// been open. For a lookup whose reads are the key of what it found (a stored goal's answer)
/// and are nothing of a miss, which the query computes otherwise and captures on its own.
pub fn recorded_if<D: Database, T>(
    db: &D,
    f: impl FnOnce() -> T,
    keep: impl FnOnce(&T) -> bool,
) -> T {
    let mut restore = Unrecorded::open(db);
    let value = f();
    if keep(&value)
        && let Some(restore) = restore.as_mut()
    {
        restore.keep = true;
    }
    value
}

/// The running query's frame as an [`unrecorded`] guard found it, put back when it ends (an
/// unwind out of its `f` too): its edges above `edges` dropped, and what those reads folded
/// into it.
struct Unrecorded {
    at: u32,
    serial: u64,
    edges: u32,
    dedup_floor: u32,
    dedup_at: u32,
    dropped: u64,
    durability: u8,
    changed: u32,
    summary: Summary,
    head: u32,
    unshareable: bool,
    untracked: bool,
    stored: bool,
    /// [`recorded_if`]'s `keep` held: only the dedup floor is put back.
    keep: bool,
}

impl Unrecorded {
    /// Opens the guard on `db`'s running query; `None` with none recording (a barrier on top,
    /// no query), where nothing is recorded to keep out.
    fn open<D: Database>(db: &D) -> Option<Unrecorded> {
        let storage = db.storage();
        local::with(|local| {
            let edges = local.edges.len() as u32;
            let at = local.frames.len().checked_sub(1)? as u32;
            let frame = local.recording(storage)?;
            let open = Unrecorded {
                at,
                serial: frame.serial,
                edges,
                dedup_floor: frame.dedup_floor,
                dedup_at: frame.dedup_at,
                dropped: frame.dropped,
                durability: frame.durability,
                changed: frame.changed,
                summary: frame.summary,
                head: frame.head,
                unshareable: frame.unshareable,
                untracked: frame.untracked,
                stored: frame.stored,
                keep: false,
            };
            frame.dedup_floor = edges;
            Some(open)
        })
    }
}

impl Drop for Unrecorded {
    fn drop(&mut self) {
        local::with(|local| {
            let frame = local.frames.get(self.at as usize);
            // A stack that is not as the guard found it is the engine's bug: said, unless an
            // unwind is already under way (a panic in a drop then would abort, naming nothing).
            if frame.is_none_or(|it| it.serial != self.serial) {
                assert!(
                    std::thread::panicking(),
                    "blaze-query: an `unrecorded` guard opened on the run at stack index {} \
                     (serial {}) ended with another run there: the stack was not unwound in order",
                    self.at,
                    self.serial,
                );
                return;
            }
            #[cfg(test)]
            let keep = self.keep || probe::UNRECORDED_KEEPS.get();
            #[cfg(not(test))]
            let keep = self.keep;
            if keep {
                local.frames[self.at as usize].dedup_floor = self.dedup_floor;
                return;
            }
            local.truncate_edges(self.edges as usize);
            let frame = &mut local.frames[self.at as usize];
            frame.dedup_floor = self.dedup_floor;
            frame.dedup_at = self.dedup_at;
            frame.dropped = self.dropped;
            frame.durability = self.durability;
            frame.changed = self.changed;
            frame.summary = self.summary;
            // A cycle met through the stack stays the run's: see `unrecorded`.
            frame.head = self.head.min(frame.head);
            frame.unshareable = self.unshareable;
            frame.untracked = self.untracked;
            frame.stored = self.stored;
        });
    }
}

/// The planted bug of the guard's oracle (`tests::unrecorded`).
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        /// An [`super::unrecorded`] guard puts nothing back: its reads stay the run's and its
        /// captures', as if it were never open.
        pub(crate) static UNRECORDED_KEEPS: Cell<bool> = const { Cell::new(false) };
    }
}

/// Makes every capture frame open in the running query give `None`: the caller used something
/// the query computed earlier, outside those frames, whose reads they did not make.
pub fn unshareable_reads<D: Database>(db: &D) {
    let storage = db.storage();
    local::with(|local| {
        if let Some(frame) = local.recording(storage) {
            frame.unshareable = true;
        }
    });
}
