//! Frames that are no row's: a witnessed run, whose reads below `Durability::HIGH` its caller
//! keeps as keys, and a covered run, held to `HIGH` reads, which records none.
//!
//! A value that is read once (a describe the walk keeps) or that its inputs cannot change in
//! a build (a summary of a sealed crate's instance) need not be an engine row: no node, no
//! edges, no reverse edges. What a row gives it, that its reads are known, is given here
//! another way:
//!
//! - [`witnessed`] runs a closure in a witness frame. Reads land there as in a run's frame
//!   (a persistent query asked inside runs as ever, and its row is the read), and at the end
//!   the frame's reads below `HIGH` are handed to the caller as keys ([`Storage::key_of`]),
//!   which it looks up in [`super::changed_keys_since`] later: a key named there is a read
//!   that changed. Reads at `HIGH` are the durable view's: while
//!   [`Storage::durable_view`] is the one the caller saw, none of them changed. The run below
//!   gets none of the reads; its row does not depend on them (the caller answers for them).
//! - [`covered`] runs a closure held to `HIGH` reads: one below it (a persistent row of a
//!   lower durability, an input field, a replay that brings one) panics naming the key read,
//!   so a cover a caller holds (one row that stands for every `HIGH` input) is checked by the
//!   engine, not audited.
//!
//! What a witness cannot give keys for makes it not [`Witness::complete`]: an untracked read,
//! a provisional row of a cycle, [`super::unshareable_reads`], a node with no key. A covered
//! run refuses each of them by name instead.
//!
//! Either frame tells the run below it, as it ends, that it read what no capture or scoped
//! run open on that run holds: their values are handed out and not kept
//! ([`super::capture_reads`] gives `None`). And it hands on a cycle head it met: the run below
//! read a provisional value through it.

use super::{
    Database, Storage, Untracked,
    local::{self, Kind, Local, NONE, Pushed},
    storage::FIELD,
};

/// What a witnessed run could not give keys for: an untracked read, a provisional row, a node
/// with no key, [`super::unshareable_reads`]. `false`: its caller asks it again each build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Witness {
    pub complete: bool,
}

impl Witness {
    /// The read a key [`witnessed`] gave names: `` `query`(key) `` or `` `Input.field`(Id) ``,
    /// for the span or error of a caller that says what a run read below `Durability::HIGH`.
    /// Written under a barrier: what a key's `Debug` reads is recorded into no frame.
    pub fn read_text<D: Database>(db: &D, key: u64) -> String {
        let storage = db.storage();
        let barrier = local::barrier();
        let text = super::node_text(storage, key);
        drop(barrier);
        text
    }

    /// The chain of reads that put `key`, a key [`witnessed`] gave, below `Durability::HIGH`:
    /// from `key`'s row its first edge below `HIGH`, from that one's row the same, down to an
    /// input or tracked field (or a row that read nothing below `HIGH`, low for an untracked
    /// read), each as [`Witness::read_text`] names it, joined by ` < `: what a span names as the
    /// read's cause, so a profile says which input made a run low, not only which row it read.
    /// At most 64 reads are followed (`CAUSE_DEPTH`); a longer chain ends in `< ...`. Written
    /// under a barrier, as [`Witness::read_text`].
    pub fn cause_text<D: Database>(db: &D, key: u64) -> String {
        let storage = db.storage();
        let barrier = local::barrier();
        let mut text = super::node_text(storage, key);
        let mut edges = Vec::new(); // alloc: one row's edges at a time, once a span's cause
        let mut at = key;
        for depth in 0.. {
            let (index, id, _) = super::storage::key_parts(at);
            // A field's key ends the chain: it is the input read.
            let row = super::ingredient::vtable(index)
                .is_some_and(|it| it.kind == super::ingredient::Kind::Function);
            if !row {
                break;
            }
            let Some(header) = super::function::header_of(storage, index, id) else { break };
            storage.copy_edges(header.edges.load(std::sync::atomic::Ordering::Acquire), &mut edges);
            let Some(low) = edges.iter().copied().find(|&node| {
                durability_of(storage, node).is_some_and(|it| it < super::Durability::HIGH.0)
            }) else {
                break;
            };
            if depth == CAUSE_DEPTH {
                text.push_str(" < ...");
                break;
            }
            at = storage.key_of(low);
            if at == 0 {
                text.push_str(" < a node with no key");
                break;
            }
            text.push_str(" < ");
            text.push_str(&super::node_text(storage, at));
        }
        drop(barrier);
        text
    }
}

/// How many reads [`Witness::cause_text`] follows below the key it is given.
const CAUSE_DEPTH: usize = 64;

/// What a witness frame is named in a runaway or an error.
const WITNESSED: &str = "a witnessed run";

/// Runs `f` with its reads recorded into a frame that is no row's: the key
/// ([`Storage::key_of`]) of each node read below `Durability::HIGH`, each once, sorted, into
/// `reads` (cleared first). A persistent query running below gets none of them: the caller
/// answers for what `f` read (it keeps the keys, or a cover). Under a witness frame of either
/// mode it pushes none and leaves `reads` as it is: under a witnessed one the reads land in
/// that one's, and this caller can name none of them (`complete` is `false`); under a covered
/// one the cover's rule holds for them (every read is at `HIGH`, `complete` is `true`).
pub fn witnessed<D: Database, R>(db: &D, reads: &mut Vec<u64>, f: impl FnOnce() -> R) -> (R, Witness) {
    let storage = db.storage();
    #[cfg(test)]
    if probe::PUSHES_NONE.with(std::cell::Cell::get) {
        // The planted bug: the reads land in the run below, as if it had made them.
        reads.clear();
        return (f(), Witness { complete: true });
    }
    let at = match local::with(|local| local.push_witness(storage, false, WITNESSED)) {
        Pushed::At(at) => at,
        Pushed::UnderWitness => return (f(), Witness { complete: false }),
        Pushed::UnderCovered => return (f(), Witness { complete: true }),
    };
    let value = run_in(at, f);
    // Sized outside the stack's lend: an allocation may wait under the process's cap.
    let held = local::with(|local| local.edges.len() - local.frames[at as usize].edges_start as usize);
    reads.clear();
    reads.reserve(held); // alloc: the caller's buffer, kept per thread by its caller
    let complete = local::with(|local| {
        let frame = &local.frames[at as usize];
        let mut complete = !frame.untracked && !frame.unshareable && frame.head == NONE;
        for &node in &local.edges[frame.edges_start as usize..] {
            match storage.key_of(node) {
                0 => complete = false,
                // alloc: none, reserved above
                key => reads.push(key),
            }
        }
        end(local, at);
        complete
    });
    reads.sort_unstable();
    reads.dedup();
    (value, Witness { complete })
}

/// Runs `f` held to `Durability::HIGH` reads: a read below it, an untracked read or a
/// provisional row panics naming the key read ([`super::key_text`]) and `what`. Recorded
/// nowhere. Under a covered frame it pushes none (the rule holds already).
///
/// # Panics
///
/// As said, naming `what`; and as `f` does.
pub fn covered<D: Database, R>(db: &D, what: &'static str, f: impl FnOnce() -> R) -> R {
    let storage = db.storage();
    let at = match local::with(|local| local.push_witness(storage, true, what)) {
        Pushed::At(at) => at,
        Pushed::UnderCovered => return f(),
        Pushed::UnderWitness => unreachable!("a covered frame goes on a witnessed one"),
    };
    let value = run_in(at, f);
    let refused = local::with(|local| {
        let frame = &local.frames[at as usize];
        let refused = if frame.head != NONE {
            let head = &local.frames[frame.head as usize];
            Some(Refused::Provisional { ingredient: head.ingredient, id: head.id })
        } else if frame.unshareable {
            Some(Refused::Unshareable)
        } else {
            None
        };
        end(local, at);
        refused
    });
    if let Some(refused) = refused {
        refuse(storage, what, refused);
    }
    value
}

/// Runs `f` over the witness frame at `at`, which is ended (its head handed on, its frame
/// popped) should `f` unwind; a normal return leaves it for its caller to end.
fn run_in<R>(at: u32, f: impl FnOnce() -> R) -> R {
    /// Ends the frame of an unwinding run: the frames above it ended first, as the unwind
    /// passed their guards.
    struct Unwind {
        at: u32,
        armed: bool,
    }

    impl Drop for Unwind {
        fn drop(&mut self) {
            if self.armed {
                let at = self.at;
                local::with(|local| {
                    if local.frames.len() > at as usize {
                        end(local, at);
                    }
                });
            }
        }
    }

    let mut unwind = Unwind { at, armed: true };
    let value = f();
    unwind.armed = false;
    value
}

/// Ends the witness frame at `at` (the top frame, but on an unwind that left frames above
/// it, which go with it): the run below (a run or a witness of any storage) is told a cycle
/// head the frame met, and that it read what the frame's reads stand for, which no capture or
/// scoped run open on it holds; then the frame and its reads go.
fn end(local: &mut Local, at: u32) {
    let head = local.frames[at as usize].head;
    let below = at.checked_sub(1);
    let captured = below.is_some_and(|below| local.captures.last().is_some_and(|it| it.frame == below));
    if let Some(below) = below
        && let Some(frame) = local.frames.get_mut(below as usize)
    {
        match frame.kind {
            Kind::Run => {
                frame.head = frame.head.min(head);
                frame.unshareable = true;
            }
            Kind::Witness { .. } => {
                frame.head = frame.head.min(head);
                // A witness below needs none of a cover's reads (all at `HIGH`), but a capture
                // open on it must hold every read it is replayed for.
                frame.unshareable |= head != NONE || captured;
            }
            Kind::Verify | Kind::Barrier { .. } => {}
        }
    }
    local.pop(at);
}

/// Why a covered run is refused at its end.
enum Refused {
    /// It read a provisional row of the cycle headed by this row.
    Provisional { ingredient: u32, id: u32 },
    /// It was told [`super::unshareable_reads`], or a capture or scoped run inside it was.
    Unshareable,
}

#[cold]
#[inline(never)]
fn refuse(storage: &Storage, what: &'static str, refused: Refused) -> ! {
    match refused {
        Refused::Provisional { ingredient, id } => {
            let barrier = local::barrier();
            let key = super::key_text(storage, ingredient, super::Id::from_u32(id));
            drop(barrier);
            use blaze_report::say::{Code, Words};
            let said = blaze_report::say!(
                Words(what),
                ": held to ",
                Code("Durability::HIGH"),
                " reads (",
                Code("blaze_query::db::covered"),
                "), it read a provisional row of the cycle headed by ",
                Code(super::ingredient::name(ingredient)),
                Words(&format!("({key})")), // alloc: the panic's text
                ": a cover stands for values, not a round of one",
            );
            panic!("{}", said.flat())
        }
        Refused::Unshareable => panic!(
            "{what}: held to `Durability::HIGH` reads (`blaze_query::db::covered`), it was told \
             its reads may not be shared (`unshareable_reads`): a cover stands for every read"
        ),
    }
}

/// The panic of a read below `Durability::HIGH` in a covered frame ([`local::Note::Uncovered`]),
/// naming the node read and what the frame on top is held for.
#[cold]
#[inline(never)]
pub(crate) fn uncovered(storage: &Storage, node: u32) -> ! {
    let what = local::with(|local| local.frames.last().map_or("", |it| it.what));
    let read = if node == 0 {
        "a replay's reads, below it by their durability".to_owned() // alloc: a panic's text
    } else {
        // Written under a barrier: what a key's `Debug` reads is recorded into no frame.
        let barrier = local::barrier();
        let read = super::node_text(storage, storage.key_of(node));
        drop(barrier);
        read
    };
    panic!(
        "{what}: held to `Durability::HIGH` reads (`blaze_query::db::covered`), it read {read} \
         below it"
    )
}

/// The panic of an untracked read in a covered frame ([`super::report_untracked_read`]).
#[cold]
#[inline(never)]
pub(crate) fn refuse_untracked(what: &'static str, why: Untracked) -> ! {
    panic!(
        "{what}: held to `Durability::HIGH` reads (`blaze_query::db::covered`), it made an \
         untracked read: {}",
        why.what
    )
}

/// The durability of `node`: an input or tracked field's own, a function row's as it last ran;
/// `None` for a node of neither (no key, a row of no function).
pub(crate) fn durability_of(storage: &Storage, node: u32) -> Option<u8> {
    let high = (storage.color_of(node) >> 32) as u32;
    if high >= FIELD {
        return Some((high & !FIELD) as u8);
    }
    super::frontier::row_of(storage, node).map(|(header, ..)| header.durability())
}

#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        /// The planted bug: on this thread a witness frame skips every read (`skip_at` 0), so
        /// its caller is given no key of what changed.
        pub(crate) static SKIPS_EVERY_READ: Cell<bool> = const { Cell::new(false) };
        /// The planted bug: on this thread `witnessed` pushes no frame, so its reads land in
        /// the run below, as a run's own reads would.
        pub(crate) static PUSHES_NONE: Cell<bool> = const { Cell::new(false) };
    }
}
