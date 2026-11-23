//! The loop-entry variant (WAVE-BACKEND.md piece L; backend-next.md D98, D100 points 2 and 3):
//! the same LIR as a fast function, entered at one of its loop headers with the loop map's
//! values as its parameters, so a running loop tiers up at a back edge's poll.
//!
//! # The convention (one place; blaze-launch-rt poll.rs is its mirror)
//!
//! The caller hands the header's loop-map KEYS in the record's order (sorted, each once:
//! blaze-tier record/form.rs, "each header's live values by value"), in the one key space of
//! blaze-emit compose/loop_map.rs: a key below the function's instruction count is that value;
//! a key at or past it is the stack slot `key - count` whose address was taken.
//!
//! The variant takes ONE pointer-sized integer WORD per key, in key order, under the
//! function's own calling convention, and returns what the function returns:
//!
//! - a value's word is the value ZERO-EXTENDED to the word, the form a fast record keeps in a
//!   register or a cell (blaze-emit compose/emit.rs, "Where values live"): an integer narrower
//!   than the word is `trunc`ated back, an `f64` is the word's bits (`bitcast`), an `f32` the
//!   low 32 bits' (`trunc` then `bitcast`), a pointer is `from_exposed` (the fast frame exposed
//!   it). A value of any other type (wider integers, vectors, compounds) has no word and is
//!   refused by name ([`LoopEntryError::Unpassable`]): the fast tier gives such a value no
//!   register and no cell either.
//! - a slot's word is the ADDRESS of the slot in the fast frame (D100 point 2: a slot whose
//!   address is taken is never moved; the variant runs in a new frame above the fast frame and
//!   reaches it through the pointer). Every `slot_addr` of that slot in the variant is that
//!   pointer; its `slot_live` and `slot_dead` are dropped (the storage is the fast frame's).
//!
//! All words are integers so that the fault handler places each in the next integer argument
//! register (then the stack) knowing only the row's place, never the value's type: a cell does
//! not say whether it holds an integer or a float.
//!
//! # The function
//!
//! Block 0 is the variant's own head: its parameters (the words), each word made back into its
//! value, then a `jump` to the header with the header's parameters (an unused header
//! parameter, which the loop map does not list, is `uninit`). Then every block reached from the
//! header, in the function's order, copied. Block 0 of the function is never reached from a
//! header (it has no predecessor); a header that reaches it is refused by name.
//!
//! A value used in the copied blocks is one of:
//!
//! - made in a copied block: copied with it;
//! - made outside them and listed: the head's value for its word (the definition dominates the
//!   header, so the head stands in for it everywhere);
//! - made outside them and NOT listed: refused by name ([`LoopEntryError::NotInMap`]): the loop
//!   map missed a live value, and nothing is guessed for it.
//!
//! A listed value whose definition is ALSO in a copied block (an outer loop's value, live into
//! an inner header: its definition is reached again by the outer back edge) has two
//! definitions in the variant: the head's and its own. It is DEMOTED to a slot of the
//! variant's own: the head stores the word's value, its definition stores the new one, and a
//! use in any other block loads it (once per block); a use in its own block after the
//! definition takes the definition directly. The simplify stage's scalar replacement
//! (simplify/sroa.rs) and LLVM's make it a value again. Every path to a use passes the head or
//! the definition, so the load reads the right one.
//!
//! The output is checked by the ordinary verifier in debug builds; no flag says "variant".
//!
//! Work lists only, no recursion. Every list is in the caller's allocator.

use core::{alloc::Allocator, convert::Infallible, fmt};

use blaze_own::Vec;

use crate::{
    Block, Func, Op, Ty, View,
    func::{BlockKind, Ext, NONE, ParamKind, ParamRec, SigRec, SlotKind, SlotRec},
    op::{Field, Layout, RewriteSink, Word, rewrite_fields, walk},
    verify::{VerifyError, verify},
};

/// One row of a header's loop map, by its key (the module's "The convention").
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LiveKey(pub u32);

/// A planted bug, for the tests' controls. Only test builds act on one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A demoted value's definition does not store it: a later outer iteration reads the
    /// entry's stale word.
    NoSpill,
    /// The words are taken one key late (the last row's word goes to the first key).
    RowsRotated,
}

/// Why no variant is made, naming the thing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopEntryError {
    /// The header is block 0 or no block of the function.
    Header { header: u32 },
    /// The function's entry block is reached from the header.
    EntryReached { header: u32 },
    /// A key is not after the key before it: the loop map is sorted, each key once.
    Unsorted { key: u32 },
    /// A key names neither a value nor a stack slot of the function.
    Key { key: u32 },
    /// A listed value whose type has no word (the module's "The convention").
    Unpassable { value: u32, ty: u16 },
    /// A value used in a block reached from the header, made outside them and not listed.
    NotInMap { value: u32 },
    /// A listed slot named by an instruction other than `slot_addr`, `slot_live`, `slot_dead`.
    RowSlotOperand { inst: u32 },
    /// A C-variadic function: its variant would not be.
    Variadic,
    /// More words than a signature row holds, or more rows than LIR numbers.
    TooLarge,
    /// The output broke a rule of LIR (debug builds check it).
    Output(VerifyError),
}

impl fmt::Display for LoopEntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(backend-next.md D100 point 2: the loop-entry variant)";
        match *self {
            LoopEntryError::Header { header } => write!(f, "block{header} is not a loop header of the function {rule}"),
            LoopEntryError::EntryReached { header } => {
                write!(f, "the entry block is reached from loop header block{header} {rule}")
            }
            LoopEntryError::Unsorted { key } => write!(f, "the loop map's key {key} is not after the key before it {rule}"),
            LoopEntryError::Key { key } => write!(f, "the loop map's key {key} names no value and no stack slot {rule}"),
            LoopEntryError::Unpassable { value, ty } => {
                write!(f, "v{value} (type {ty}) is live into the loop but has no word to be passed in {rule}")
            }
            LoopEntryError::NotInMap { value } => {
                write!(f, "v{value} is used in the loop, made before it, and not in its loop map {rule}")
            }
            LoopEntryError::RowSlotOperand { inst } => {
                write!(f, "instruction {inst} names a slot the fast frame keeps, other than by its address {rule}")
            }
            LoopEntryError::Variadic => write!(f, "a C-variadic function has no loop-entry variant {rule}"),
            LoopEntryError::TooLarge => write!(f, "the loop-entry variant has more words or rows than LIR numbers {rule}"),
            LoopEntryError::Output(error) => write!(f, "the loop-entry variant breaks a rule of LIR: {error} {rule}"),
        }
    }
}

impl core::error::Error for LoopEntryError {}

/// The variant of `view` entered at `header` with the words of `live` (the module's "The
/// convention"), every buffer in `alloc`. `view` must be verified (`crate::verify`).
pub fn entry_variant<A: Allocator + Copy>(
    view: View<'_>,
    header: Block,
    live: &[LiveKey],
    alloc: A,
) -> Result<Func<A>, LoopEntryError> {
    entry_variant_planted(view, header, live, Plant::None, alloc)
}

/// [`entry_variant`] with a planted bug, which only test builds act on.
pub fn entry_variant_planted<A: Allocator + Copy>(
    view: View<'_>,
    header: Block,
    live: &[LiveKey],
    plant: Plant,
    alloc: A,
) -> Result<Func<A>, LoopEntryError> {
    let plant = if cfg!(test) { plant } else { Plant::None };
    let blocks = view.blocks();
    let h = header.0 as usize;
    if h == 0 || h >= blocks {
        return Err(LoopEntryError::Header { header: header.0 });
    }
    if view.head.sig as usize >= view.sigs.len() {
        return Err(LoopEntryError::Header { header: header.0 });
    }
    if view.sigs[view.head.sig as usize].bits & SigRec::VARIADIC != 0 {
        return Err(LoopEntryError::Variadic);
    }
    let values = view.len();
    if values.checked_add(view.slots.len()).is_none_or(|it| it >= NONE as usize) {
        return Err(LoopEntryError::TooLarge);
    }
    let block_of = block_of(&view, &alloc);
    let region = reach(&view, header.0, &alloc);
    if region[0] {
        return Err(LoopEntryError::EntryReached { header: header.0 });
    }
    let rows = rows(&view, live, &alloc)?;
    let word = Ty::int(view.head.ptr_bits).ok_or(LoopEntryError::TooLarge)?;
    let header_params = view.params(header);
    // Per value: listed, and whether its definition is copied (then it is demoted, unless it
    // is a parameter of the header, which the head's `jump` passes).
    let mut listed = filled(values, false, &alloc);
    let mut demoted = filled(values, false, &alloc);
    for row in rows.iter() {
        if let Row::Value(v) = *row {
            listed[v as usize] = true;
            let in_header = header_params.contains(&(v as usize));
            if region[block_of[v as usize] as usize] && !in_header {
                demoted[v as usize] = true;
            }
        }
    }
    // Per block of the function: the invoke whose value is defined there (its normal block),
    // so a demoted invoke's value is stored at that block's start.
    let mut invoked = filled(blocks, NONE, &alloc);
    for at in 0..values {
        if matches!(view.op(at), Op::Invoke | Op::InvokeIndirect) && demoted[at] {
            let mut o = view.operands(at);
            o.word();
            o.sig();
            invoked[o.block().index()] = at as u32;
        }
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();
    func.slots.clear();
    func.variables.clear();
    // The slots: the function's own but those the fast frame keeps (listed), then one a
    // demoted value.
    let mut kept = filled(view.slots.len(), false, &alloc);
    for row in rows.iter() {
        if let Row::Slot(s) = *row {
            kept[s as usize] = true;
        }
    }
    let mut slot_row = filled(view.slots.len(), NONE, &alloc);
    for (slot, rec) in view.slots.iter().enumerate() {
        if !kept[slot] {
            slot_row[slot] = func.slots.len() as u32;
            func.slots.push(*rec);
        }
    }
    for var in view.variables {
        let row = slot_row[var.slot as usize];
        if row != NONE {
            let mut var = *var;
            var.slot = row;
            func.variables.push(var);
        }
    }
    // The signature: a word a row, the function's returns.
    let own = view.sigs[view.head.sig as usize];
    let first = u32::try_from(func.sig_params.len()).map_err(|_| LoopEntryError::TooLarge)?;
    let params = u16::try_from(rows.len()).map_err(|_| LoopEntryError::TooLarge)?;
    let word_param = ParamRec { facts: 0, ty: word.0, ext: Ext::None as u8, kind: ParamKind::Value as u8 };
    for _ in 0..rows.len() {
        func.sig_params.push(word_param);
    }
    let rets_at = own.first as usize + own.params as usize;
    for i in 0..own.rets as usize {
        func.sig_params.push(view.sig_params[rets_at + i]);
    }
    func.head.sig = u32::try_from(func.sigs.len()).map_err(|_| LoopEntryError::TooLarge)?;
    func.sigs.push(SigRec { first, params, rets: own.rets, conv: own.conv, bits: 0, pad: [0; 2] });

    // Block 0 is the head; then the blocks reached from the header, in the function's order.
    let mut block_map = filled(blocks, NONE, &alloc);
    let mut tail = 1u32;
    for b in (0..blocks).filter(|&b| region[b]) {
        block_map[b] = tail;
        tail += 1;
    }
    let mut e = Emit {
        view,
        plant,
        func,
        map: filled(values, NONE, &alloc),
        block_map,
        slot_row,
        slot_ptr: filled(view.slots.len(), NONE, &alloc),
        demoted,
        demote_addr: filled(values, NONE, &alloc),
        def_block: filled(values, NONE, &alloc),
        current: NONE,
        fetched: filled(values, NONE, &alloc),
        fetched_in: filled(values, NONE, &alloc),
        fixups: Vec::new_in(alloc),
    };
    for at in 0..values {
        e.def_block[at] = block_of[at];
    }
    for (b, &inst) in invoked.iter().enumerate() {
        if inst != NONE {
            e.def_block[inst as usize] = b as u32;
        }
    }

    // ---- the head ----
    e.begin(BlockKind::Normal);
    let mut words = Vec::with_capacity_in(rows.len(), alloc);
    for _ in 0..rows.len() {
        words.push(e.raw(Op::Param, 0, word, 0, 0, 0));
    }
    if plant == Plant::RowsRotated && !words.is_empty() {
        words.rotate_right(1);
    }
    for (i, row) in rows.iter().enumerate() {
        let w = words[i];
        match *row {
            Row::Value(v) => {
                let ty = Ty(view.tys[v as usize]);
                let made = e.from_word(w, word, ty, v)?;
                if e.demoted[v as usize] {
                    let addr = e.demote_slot(ty, view.head.ptr_bits)?;
                    e.demote_addr[v as usize] = addr;
                    e.store(made, addr, ty);
                } else {
                    e.map[v as usize] = made;
                }
            }
            Row::Slot(s) => e.slot_ptr[s as usize] = e.push(Op::FromExposed, Ty::PTR, &[w]),
        }
    }
    // The header's parameters: the head's value of each listed one, `uninit` for another.
    let mut args = Vec::with_capacity_in(header_params.len(), alloc);
    for at in header_params.start..header_params.end {
        let arg = match listed[at] {
            true => e.map[at],
            false => e.push(Op::Uninit, Ty(view.tys[at]), &[]),
        };
        args.push(arg);
    }
    let start = e.func.extra.len() as u32;
    e.func.extra.push(e.block_map[h]);
    e.func.extra.push(args.len() as u32);
    e.func.extra.extend_from_slice(&args[..]);
    e.raw(Op::Jump, 0, Ty::NONE, start, 0, 0);
    // The header's parameters are the header's own from here on.
    for at in header_params.start..header_params.end {
        e.map[at] = NONE;
    }

    // ---- the blocks reached from the header ----
    for b in (0..blocks).filter(|&b| region[b]) {
        let block = Block(b as u32);
        e.begin(view.block_kind(block));
        e.current = b as u32;
        let params = view.params(block);
        for at in params.start..params.end {
            e.map[at] = e.raw(Op::Param, view.flags[at], Ty(view.tys[at]), 0, view.attrs[at], view.locs[at]);
        }
        for at in params.start..params.end {
            e.spill(at);
        }
        if invoked[b] != NONE {
            e.spill(invoked[b] as usize);
        }
        for at in params.end..view.block(block).end {
            e.inst(at)?;
        }
    }

    // Every value named in a copied operand becomes its value here.
    let Emit { mut func, map, fixups, .. } = e;
    for &(place, in_data) in fixups.iter() {
        let word = match in_data {
            true => &mut func.data[place as usize],
            false => &mut func.extra[place as usize],
        };
        let to = map[*word as usize];
        if to == NONE {
            return Err(LoopEntryError::NotInMap { value: *word });
        }
        *word = to;
    }
    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(LoopEntryError::TooLarge);
    }
    if cfg!(debug_assertions) {
        verify(func.view()).map_err(LoopEntryError::Output)?;
    }
    Ok(func)
}

/// A row of the loop map, read.
#[derive(Clone, Copy)]
enum Row {
    Value(u32),
    Slot(u32),
}

fn filled<T: Clone, A: Allocator + Copy>(len: usize, item: T, alloc: &A) -> Vec<T, A> {
    Vec::from_elem_in(item, len, *alloc)
}

/// The keys read into rows: sorted, each once, each a value of a type with a word or a slot.
fn rows<A: Allocator + Copy>(view: &View<'_>, live: &[LiveKey], alloc: &A) -> Result<Vec<Row, A>, LoopEntryError> {
    let values = view.len();
    let mut rows = Vec::with_capacity_in(live.len(), *alloc);
    let mut last: Option<u32> = None;
    for &LiveKey(key) in live {
        if last.is_some_and(|it| it >= key) {
            return Err(LoopEntryError::Unsorted { key });
        }
        last = Some(key);
        let at = key as usize;
        if at < values {
            let ty = Ty(view.tys[at]);
            if word_form(ty, view.head.ptr_bits).is_none() {
                return Err(LoopEntryError::Unpassable { value: key, ty: ty.0 });
            }
            rows.push(Row::Value(key));
        } else if at - values < view.slots.len() {
            rows.push(Row::Slot((at - values) as u32));
        } else {
            return Err(LoopEntryError::Key { key });
        }
    }
    Ok(rows)
}

/// How a value of `ty` is made from its word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WordForm {
    Same,
    Trunc,
    F64,
    F32,
    Ptr,
}

fn word_form(ty: Ty, ptr_bits: u32) -> Option<WordForm> {
    let word = Ty::int(ptr_bits)?;
    if ty == word {
        return Some(WordForm::Same);
    }
    if ty == Ty::PTR {
        return Some(WordForm::Ptr);
    }
    if ty == Ty::F64 && ptr_bits == 64 {
        return Some(WordForm::F64);
    }
    if ty == Ty::F32 {
        return Some(WordForm::F32);
    }
    if [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64].contains(&ty) && ty.bits(ptr_bits) < ptr_bits {
        return Some(WordForm::Trunc);
    }
    None
}

/// Per instruction, its block.
fn block_of<A: Allocator + Copy>(view: &View<'_>, alloc: &A) -> Vec<u32, A> {
    let mut of = filled(view.len(), NONE, alloc);
    for b in 0..view.blocks() {
        for at in view.block(Block(b as u32)) {
            of[at] = b as u32;
        }
    }
    of
}

/// Calls `f` with each block the terminator `at` names (every edge, unwind and label too).
fn successors(view: &View<'_>, at: usize, mut f: impl FnMut(u32)) {
    let Layout::Extra(fields) = view.op(at).info().layout else { return };
    let words = &view.extra[view.data[at] as usize..];
    let mut visit = |word: Word| -> Result<(), Infallible> {
        if let Word::Block(block) = word {
            f(block);
        }
        Ok(())
    };
    let walked = walk(words, fields, Ty(view.tys[at]), view.tys, &mut visit);
    debug_assert!(walked.is_ok(), "the operands of a verified function walk");
}

/// The blocks reached from `from`, itself included.
fn reach<A: Allocator + Copy>(view: &View<'_>, from: u32, alloc: &A) -> Vec<bool, A> {
    let mut seen = filled(view.blocks(), false, alloc);
    let mut work = Vec::new_in(*alloc);
    seen[from as usize] = true;
    work.push(from);
    while let Some(b) = work.pop() {
        let block = Block(b);
        if view.block(block).is_empty() {
            continue;
        }
        successors(view, view.terminator(block), |to| {
            if !seen[to as usize] {
                seen[to as usize] = true;
                work.push(to);
            }
        });
    }
    seen
}

struct Emit<'v, A: Allocator + Copy> {
    view: View<'v>,
    plant: Plant,
    func: Func<A>,
    /// Per value of the function: its value here, [`NONE`] if nothing made it yet.
    map: Vec<u32, A>,
    /// Per block of the function: its block here, [`NONE`] if it is not copied.
    block_map: Vec<u32, A>,
    /// Per slot of the function: its row here, [`NONE`] for one the fast frame keeps.
    slot_row: Vec<u32, A>,
    /// Per slot of the function the fast frame keeps: the head's pointer to it.
    slot_ptr: Vec<u32, A>,
    demoted: Vec<bool, A>,
    /// Per demoted value: the head's address of its slot.
    demote_addr: Vec<u32, A>,
    /// Per value: the block it is defined in (an invoke's: its normal block).
    def_block: Vec<u32, A>,
    /// The function's block being copied.
    current: u32,
    /// Per demoted value: its load in the block `fetched_in` names.
    fetched: Vec<u32, A>,
    fetched_in: Vec<u32, A>,
    /// The places that hold a value of the function, to be its value here once every block
    /// is copied: an index of `data` if the flag is set, else of `extra`.
    fixups: Vec<(u32, bool), A>,
}

impl<A: Allocator + Copy> Emit<'_, A> {
    fn begin(&mut self, kind: BlockKind) {
        self.func.block_starts.push(self.func.ops.len() as u32);
        self.func.block_kinds.push(kind as u8);
    }

    fn raw(&mut self, op: Op, flags: u8, ty: Ty, data: u32, attrs: u32, loc: u32) -> u32 {
        let at = self.func.ops.len() as u32;
        self.func.ops.push(op as u8);
        self.func.flags.push(flags);
        self.func.tys.push(ty.0);
        self.func.data.push(data);
        self.func.attrs.push(attrs);
        self.func.locs.push(loc);
        at
    }

    /// An instruction of the pass's own: no flags, no memory promise (an access of alignment
    /// 1), no location; its operands `words` as its layout has them.
    fn push(&mut self, op: Op, ty: Ty, words: &[u32]) -> u32 {
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(_) => words[0],
            Layout::Extra(_) => {
                let start = self.func.extra.len() as u32;
                self.func.extra.extend_from_slice(words);
                start
            }
        };
        self.raw(op, 0, ty, data, 0, 0)
    }

    /// The value `v` of type `ty` from its word `w` (the module's "The convention").
    fn from_word(&mut self, w: u32, word: Ty, ty: Ty, v: u32) -> Result<u32, LoopEntryError> {
        let form = word_form(ty, self.view.head.ptr_bits).ok_or(LoopEntryError::Unpassable { value: v, ty: ty.0 })?;
        Ok(match form {
            WordForm::Same => w,
            WordForm::Trunc => self.push(Op::Trunc, ty, &[w]),
            WordForm::F64 => self.push(Op::Bitcast, Ty::F64, &[w]),
            WordForm::F32 => {
                let low = match word == Ty::I32 {
                    true => w,
                    false => self.push(Op::Trunc, Ty::I32, &[w]),
                };
                self.push(Op::Bitcast, Ty::F32, &[low])
            }
            WordForm::Ptr => self.push(Op::FromExposed, Ty::PTR, &[w]),
        })
    }

    /// A slot of the variant's own for a demoted value of `ty`; its address, made in the head
    /// (which dominates every block).
    fn demote_slot(&mut self, ty: Ty, ptr_bits: u32) -> Result<u32, LoopEntryError> {
        let bytes = match ty == Ty::I1 {
            true => 1,
            false => ty.bits(ptr_bits).div_ceil(8),
        };
        let row = u32::try_from(self.func.slots.len()).map_err(|_| LoopEntryError::TooLarge)?;
        self.func.slots.push(SlotRec { size: bytes, align: bytes.trailing_zeros() as u8, kind: SlotKind::Temp as u8, pad: [0; 2] });
        Ok(self.push(Op::SlotAddr, Ty::PTR, &[row]))
    }

    /// Stores `value` of `ty` at `addr`: an `i1` as a byte.
    fn store(&mut self, value: u32, addr: u32, ty: Ty) {
        let stored = match ty == Ty::I1 {
            true => self.push(Op::Zext, Ty::I8, &[value]),
            false => value,
        };
        self.push(Op::Store, Ty::NONE, &[stored, addr, 0]);
    }

    /// Stores the function's value `at`, made in this block, to its slot if it is demoted. It
    /// is named by a fixup: an invoke's value is stored in its normal block, which may come
    /// before the invoke in the function's order.
    fn spill(&mut self, at: usize) {
        if !self.demoted[at] || self.plant == Plant::NoSpill {
            return;
        }
        let (addr, ty) = (self.demote_addr[at], Ty(self.view.tys[at]));
        let start = self.func.extra.len() as u32;
        if ty == Ty::I1 {
            let byte = self.raw(Op::Zext, 0, Ty::I8, at as u32, 0, 0);
            self.fixups.push((byte, true));
            self.func.extra.push(byte);
        } else {
            self.fixups.push((start, false));
            self.func.extra.push(at as u32);
        }
        self.func.extra.extend_from_slice(&[addr, 0]);
        self.raw(Op::Store, 0, Ty::NONE, start, 0, 0);
    }

    /// Makes the demoted `value` usable in the current block: a load, once per block, in any
    /// block but its definition's.
    fn fetch(&mut self, value: u32) {
        let at = value as usize;
        if !self.demoted[at] || self.def_block[at] == self.current || self.fetched_in[at] == self.current {
            return;
        }
        let (ty, addr) = (Ty(self.view.tys[at]), self.demote_addr[at]);
        let new = match ty == Ty::I1 {
            true => {
                let byte = self.push(Op::Load, Ty::I8, &[addr, 0]);
                self.push(Op::Trunc, Ty::I1, &[byte])
            }
            false => self.push(Op::Load, ty, &[addr, 0]),
        };
        self.fetched[at] = new;
        self.fetched_in[at] = self.current;
    }

    /// Appends the operands `fields` of instruction `inst` to `extra`, each value, block and
    /// slot as the variant names it.
    fn rewrite(&mut self, inst: usize, fields: &'static [Field]) -> Result<(), LoopEntryError> {
        let view = self.view;
        let words = &view.extra[view.data[inst] as usize..];
        let ty = Ty(view.tys[inst]);
        rewrite_fields(words, fields, ty, view.tys, &mut LoopRewrite { emit: self, inst })
    }

    /// Copies instruction `at`, its operands as the variant names them.
    fn copy(&mut self, at: usize) -> Result<u32, LoopEntryError> {
        let view = self.view;
        let op = view.op(at);
        let new = self.func.ops.len() as u32;
        let data = match op.info().layout {
            Layout::None => 0,
            Layout::Data(field) => {
                let word = view.data[at];
                match field {
                    Field::Value if self.fetched_in[word as usize] == self.current => self.fetched[word as usize],
                    Field::Value => {
                        self.fixups.push((new, true));
                        word
                    }
                    Field::Slot => {
                        let row = self.slot_row[word as usize];
                        if row == NONE {
                            return Err(LoopEntryError::RowSlotOperand { inst: at as u32 });
                        }
                        row
                    }
                    Field::Block => self.block_map[word as usize],
                    _ => word,
                }
            }
            Layout::Extra(fields) => {
                let start = self.func.extra.len() as u32;
                self.rewrite(at, fields)?;
                start
            }
        };
        Ok(self.raw(op, view.flags[at], Ty(view.tys[at]), data, view.attrs[at], view.locs[at]))
    }

    /// The function's instruction `at`, not a parameter, as the variant has it.
    fn inst(&mut self, at: usize) -> Result<(), LoopEntryError> {
        let view = self.view;
        let op = view.op(at);
        if let Op::SlotAddr | Op::SlotLive | Op::SlotDead = op {
            let slot = view.data[at] as usize;
            if self.slot_row[slot] == NONE {
                // A slot the fast frame keeps: its address is the head's pointer, its storage
                // the fast frame's.
                if op == Op::SlotAddr {
                    self.map[at] = self.slot_ptr[slot];
                }
                return Ok(());
            }
        }
        each_value(&view, at, |value| self.fetch(value));
        self.map[at] = self.copy(at)?;
        // An invoke's value is defined in its normal block and stored there.
        if !matches!(op, Op::Invoke | Op::InvokeIndirect) {
            self.spill(at);
        }
        Ok(())
    }
}

struct LoopRewrite<'e, 'v, A: Allocator + Copy> {
    emit: &'e mut Emit<'v, A>,
    inst: usize,
}

impl<A: Allocator + Copy> RewriteSink for LoopRewrite<'_, '_, A> {
    type Error = LoopEntryError;

    fn put_value(&mut self, value: u32) {
        let e = &mut *self.emit;
        if e.fetched_in[value as usize] == e.current {
            e.func.extra.push(e.fetched[value as usize]);
        } else {
            e.fixups.push((e.func.extra.len() as u32, false));
            e.func.extra.push(value);
        }
    }

    fn put_block(&mut self, block: u32) {
        // Every block a copied block names is reached from the header, so it is copied.
        self.emit.func.extra.push(self.emit.block_map[block as usize]);
    }

    fn put_slot(&mut self, slot: u32) -> Result<(), LoopEntryError> {
        let row = self.emit.slot_row[slot as usize];
        if row == NONE {
            return Err(LoopEntryError::RowSlotOperand { inst: self.inst as u32 });
        }
        self.emit.func.extra.push(row);
        Ok(())
    }

    fn push_word(&mut self, word: u32) {
        self.emit.func.extra.push(word);
    }

    fn push_words(&mut self, words: &[u32]) {
        self.emit.func.extra.extend_from_slice(words);
    }
}

/// Calls `f` with each value instruction `at` uses.
fn each_value(view: &View<'_>, at: usize, mut f: impl FnMut(u32)) {
    match view.op(at).info().layout {
        Layout::None => {}
        Layout::Data(field) => {
            if field == Field::Value {
                f(view.data[at]);
            }
        }
        Layout::Extra(fields) => {
            let words = &view.extra[view.data[at] as usize..];
            let mut visit = |word: Word| -> Result<(), Infallible> {
                if let Word::Value(value) = word {
                    f(value);
                }
                Ok(())
            };
            let walked = walk(words, fields, Ty(view.tys[at]), view.tys, &mut visit);
            debug_assert!(walked.is_ok(), "the operands of a verified function walk");
        }
    }
}
