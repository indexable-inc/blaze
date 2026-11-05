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
    ingredient::{self, Kind},
    local::{self, Capture, walk_capture},
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
/// `write` must not ask for a query that may be on this thread's stack: the cycle through it
/// would make the running query a member of a cycle whose read this forgets.
pub fn for_span_text<D: Database>(db: &D, out: &mut String, write: impl FnOnce(&D, &mut String)) {
    let _restore = untracked(db);
    write(db, out);
}

/// Stops recording reads into `db`'s running query until the guard drops.
fn untracked<D: Database>(db: &D) -> Option<Unrecorded> {
    let storage = db.storage();
    local::with(|local| {
        let edges = local.edges.len() as u32;
        let at = local.frames.len().checked_sub(1)? as u32;
        let frame = local.recording(storage)?;
        Some(Unrecorded {
            at,
            edges,
            dedup_floor: frame.dedup_floor,
            durability: frame.durability,
            changed: frame.changed,
            summary: frame.summary,
            head: frame.head,
            unshareable: frame.unshareable,
            untracked: frame.untracked,
        })
    })
}

/// The running query's frame as [`for_span_text`] found it, put back when it ends (an unwind
/// out of its `write` too): its edges above `edges` dropped, and what those reads folded into it.
struct Unrecorded {
    at: u32,
    edges: u32,
    dedup_floor: u32,
    durability: u8,
    changed: u32,
    summary: Summary,
    head: u32,
    unshareable: bool,
    untracked: bool,
}

impl Drop for Unrecorded {
    fn drop(&mut self) {
        local::with(|local| {
            local.truncate_edges(self.edges as usize);
            let Some(frame) = local.frames.get_mut(self.at as usize) else { return };
            frame.dedup_floor = self.dedup_floor;
            frame.durability = self.durability;
            frame.changed = self.changed;
            frame.summary = self.summary;
            frame.head = self.head;
            frame.unshareable = self.unshareable;
            frame.untracked = self.untracked;
        });
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
