//! A RECORDER WITH OWNERS (lane RH6, "one table of frozen heads a request"): ONE recorder a
//! job, through which many cells are frozen by one shared source ([`super::head::SeededHeads`]),
//! each read kept under the cell whose making read it, so one source can serve many head tasks
//! and each task's read list is still exactly its own.
//!
//! WHO OWNS A READ: a stack of owners. A head task enters its own cell before it asks the source
//! (its own lookups: the item's text, `cell_of`); the source enters each cell it freezes for the
//! length of that freeze's attempt (head.rs `SeededHeads::drive`, innermost last) and leaves it
//! after. A read belongs to the innermost owner when it is offered: the lookups made to FIND
//! another head (its `cell_of`, its members' texts, seeding it from its frozen bytes) belong to
//! the asker; that head's own inner lookups, made while it is frozen, belong to it. A cell
//! frozen once and asked again later reads nothing again: what it read is already its own.
//!
//! BOUNDS (rule 18): each owner keeps at most [`MOST_READS`] distinct reads (as one task's
//! recorder does), at most [`MOST_OWNERS`] owners, the stack at most [`MOST_DEPTH`] deep; past
//! any, every later answer is the typed [`OwnedError`]. A read offered with no owner entered is
//! no one's, so it is refused too ([`OwnedError::Unowned`]): never a read dropped.
//!
//! COST: as [`super::recorder::Recorder`]: a repeat is one compare against the last read or one
//! probe of `seen`; a distinct read one push into its owner's list (one list an owner, amortized).
//!
//! THE PLANT (rule 23, [`Owning::Outermost`]): every read to the OUTERMOST owner on the stack,
//! as one recorder a head task kept before (all of a freeze's needs' reads in the asker's list).
//! owned_reads_tests.rs shows a cell's reads from a shared source equal its reads from a source
//! that froze it alone, and that the plant breaks the equality.

use core::cell::{Cell, RefCell};

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_scope::cell_key::BindingKeys;
use blaze_scope::names_cells::{CellSink, NameCell};
use blaze_surface::Namespace;

use super::head::{MOST_MAKING, WANT_BYTES, want_bytes};
use super::recorder::{MOST_READS, ReadSink, ReadsFull, TableRead};
use crate::front2::heads::Want;

/// Who the reads offered now belong to: what a [`super::head::SeededHeads`] tells while it
/// freezes (head.rs `SeededHeadsOpts::owners`).
pub trait ReadOwners {
    /// Cell `cell`'s stretch begins (its freeze, or its asker's own lookups): every read offered
    /// until the matching [`ReadOwners::leave`] is its, unless a cell entered inside owns it.
    fn enter(&self, cell: Want);
    /// The innermost stretch ends.
    fn leave(&self);
}

/// The most owners one recorder keeps (rule 18): a job's distinct cells.
pub const MOST_OWNERS: usize = 1 << 20;
/// The deepest owner stack: a head task's own cell, then a source's whole making stack.
pub const MOST_DEPTH: usize = MOST_MAKING + 1;

/// Who a read is kept under: the rule, or the planted bug of the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owning {
    /// The innermost owner (module docs).
    Each,
    /// PLANTED BUG, for tests only: the outermost owner on the stack takes every read.
    Outermost,
}

/// Why an [`OwnedRecorder`] cannot say an owner's reads (each typed, rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OwnedError {
    /// The owner read more than one task's recorder keeps.
    Reads(ReadsFull),
    /// More owners than [`MOST_OWNERS`].
    Owners { most: usize },
    /// An owner entered deeper than [`MOST_DEPTH`].
    Depth { most: usize },
    /// A read offered while no owner was entered, or a leave with none entered: a read that is
    /// no cell's, so every answer after it is refused.
    Unowned,
}

impl core::fmt::Display for OwnedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OwnedError::Reads(full) => write!(f, "one cell's freeze: {full}"),
            OwnedError::Owners { most } => write!(f, "the job froze more than {most} cells through one recorder (front2 tables/owned_reads.rs `MOST_OWNERS`)"),
            OwnedError::Depth { most } => write!(f, "the owners of reads were entered more than {most} deep (front2 tables/owned_reads.rs `MOST_DEPTH`)"),
            OwnedError::Unowned => f.write_str("a read was offered while no cell owned it (front2 tables/owned_reads.rs `OwnedError::Unowned`)"),
        }
    }
}

/// What an [`OwnedRecorder`] is made from.
pub struct OwnedRecorderOpts<'a> {
    /// The job's allocator: the lists live there for the job's length.
    pub alloc: Alloc<'a>,
    /// The rule, or the tests' plant.
    pub owning: Owning,
}

/// One owner's reads: distinct, in the order first offered; sorted when asked
/// ([`OwnedRecorder::reads_of`]).
struct Slot<'a> {
    reads: AVec<'a, TableRead>,
    full: bool,
}

/// A recorder whose reads are kept by owner (module docs).
pub struct OwnedRecorder<'a> {
    alloc: Alloc<'a>,
    owning: Owning,
    /// Each owner's slot, by its want's bytes.
    by_owner: RefCell<blaze_hash::HashMap<[u8; WANT_BYTES], u32, Alloc<'a>>>,
    slots: RefCell<AVec<'a, Slot<'a>>>,
    /// Every (slot, read) kept, for the repeat's O(1) answer.
    seen: RefCell<blaze_hash::HashSet<(u32, TableRead), Alloc<'a>>>,
    /// The owners entered, innermost last, by slot.
    stack: RefCell<AVec<'a, u32>>,
    /// The (slot, read) offered last: a run of one cell answered by this compare alone.
    last: Cell<Option<(u32, TableRead)>>,
    /// Reads offered, repeats too.
    offered: Cell<u64>,
    /// The recorder's own refusal ([`OwnedError::Owners`], `Depth`, `Unowned`): every answer
    /// after it is refused.
    broken: Cell<Option<OwnedError>>,
    /// The binding reads' cell keys, each derived once whoever owns the read (a key is a
    /// function of the read alone): a names lookup hashes nothing here on a repeat.
    binding_keys: RefCell<BindingKeys<'a>>,
}

impl<'a> OwnedRecorder<'a> {
    pub fn new(o: OwnedRecorderOpts<'a>) -> OwnedRecorder<'a> {
        OwnedRecorder {
            alloc: o.alloc,
            owning: o.owning,
            by_owner: RefCell::new(blaze_hash::HashMap::new_in(o.alloc)),
            slots: RefCell::new(AVec::new_in(o.alloc)),
            seen: RefCell::new(blaze_hash::HashSet::new_in(o.alloc)),
            stack: RefCell::new(AVec::new_in(o.alloc)),
            last: Cell::new(None),
            offered: Cell::new(0),
            broken: Cell::new(None),
            binding_keys: RefCell::new(BindingKeys::new_in(o.alloc)),
        }
    }

    /// How many binding cell keys it derived: one per distinct binding read.
    pub fn binding_keys_derived(&self) -> u64 {
        self.binding_keys.borrow().derived()
    }

    /// Reads offered so far, repeats too.
    pub fn offered(&self) -> u64 {
        self.offered.get()
    }

    /// How many owners it keeps.
    pub fn owners(&self) -> usize {
        self.slots.borrow().len()
    }

    /// Whether `cell` owns a read (was entered and read something, or was entered at all).
    pub fn has(&self, cell: Want) -> bool {
        self.by_owner.borrow().contains_key(&want_bytes(cell))
    }

    /// The recorder's own refusal, if any ([`OwnedError`]).
    pub fn broken(&self) -> Option<OwnedError> {
        self.broken.get()
    }

    /// Cell `cell`'s reads, each once, sorted (rule 25), given to `each`; none for an owner never
    /// entered. Refused when the recorder broke or the owner's list is past its cap.
    pub fn reads_of(&self, cell: Want, each: &mut dyn FnMut(TableRead)) -> Result<(), OwnedError> {
        if let Some(why) = self.broken.get() {
            return Err(why);
        }
        let Some(at) = self.by_owner.borrow().get(&want_bytes(cell)).copied() else { return Ok(()) };
        let slots = self.slots.borrow();
        let slot = &slots[at as usize];
        if slot.full {
            return Err(OwnedError::Reads(ReadsFull { most: MOST_READS }));
        }
        let mut sorted: AVec<'a, TableRead> = AVec::with_capacity_in(slot.reads.len(), self.alloc);
        sorted.extend_from_slice(&slot.reads);
        sorted.sort_unstable();
        for one in sorted.iter() {
            each(*one);
        }
        Ok(())
    }

    /// The slot of `cell`, made on its first entry.
    fn slot(&self, cell: Want) -> Option<u32> {
        let key = want_bytes(cell);
        if let Some(at) = self.by_owner.borrow().get(&key).copied() {
            return Some(at);
        }
        let mut slots = self.slots.borrow_mut();
        if slots.len() >= MOST_OWNERS {
            self.broken.set(Some(OwnedError::Owners { most: MOST_OWNERS }));
            return None;
        }
        let at = u32::try_from(slots.len()).ok()?;
        slots.push(Slot { reads: AVec::new_in(self.alloc), full: false });
        self.by_owner.borrow_mut().insert(key, at);
        Some(at)
    }

    /// The slot the read offered now goes to: the innermost owner, or (the plant) the outermost.
    fn owner_now(&self) -> Option<u32> {
        let stack = self.stack.borrow();
        match self.owning {
            Owning::Each => stack.last().copied(),
            Owning::Outermost => stack.first().copied(),
        }
    }
}

impl ReadOwners for OwnedRecorder<'_> {
    fn enter(&self, cell: Want) {
        if self.stack.borrow().len() >= MOST_DEPTH {
            self.broken.set(Some(OwnedError::Depth { most: MOST_DEPTH }));
            return;
        }
        // Past a refusal nothing is answered, so nothing need be entered.
        let Some(at) = self.slot(cell) else { return };
        self.stack.borrow_mut().push(at);
        self.last.set(None);
    }

    fn leave(&self) {
        if self.broken.get().is_some() {
            // A refused enter pushed nothing: the stack stays as it was.
            self.stack.borrow_mut().pop();
            return;
        }
        if self.stack.borrow_mut().pop().is_none() {
            self.broken.set(Some(OwnedError::Unowned));
        }
        self.last.set(None);
    }
}

impl ReadSink for OwnedRecorder<'_> {
    fn read(&self, read: TableRead) {
        self.offered.set(self.offered.get().saturating_add(1));
        if self.broken.get().is_some() {
            return;
        }
        let Some(owner) = self.owner_now() else {
            self.broken.set(Some(OwnedError::Unowned));
            return;
        };
        let one = (owner, read);
        if self.last.get() == Some(one) {
            return;
        }
        self.last.set(Some(one));
        let mut seen = self.seen.borrow_mut();
        let mut slots = self.slots.borrow_mut();
        let slot = &mut slots[owner as usize];
        if slot.reads.len() == MOST_READS {
            if !seen.contains(&one) {
                slot.full = true;
            }
            return;
        }
        if seen.insert(one) {
            slot.reads.push(read);
        }
    }

    fn binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        let key = self.binding_keys.borrow_mut().key(module, ns, name).key;
        self.read(TableRead::Names(NameCell::Binding { module, name: key }));
    }
}

impl CellSink for OwnedRecorder<'_> {
    fn put(&self, cell: NameCell) {
        self.read(TableRead::Names(cell));
    }

    fn put_binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        ReadSink::binding(self, module, ns, name);
    }
}

/// Two recorders entered and left as one (a freeze's names reads and its constants'
/// evaluation's, kept apart as blaze-cli engine_head_consts.rs keeps them).
pub struct BothOwners<'r, 'a> {
    pub names: &'r OwnedRecorder<'a>,
    pub evals: &'r OwnedRecorder<'a>,
}

impl ReadOwners for BothOwners<'_, '_> {
    fn enter(&self, cell: Want) {
        self.names.enter(cell);
        self.evals.enter(cell);
    }

    fn leave(&self) {
        self.names.leave();
        self.evals.leave();
    }
}
