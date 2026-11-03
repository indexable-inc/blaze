//! Literal pools, and the other things a backend asks the core to do to a
//! section.
//!
//! `ldr r0, =0x12345678` loads a constant no instruction can hold, so the
//! assembler puts the constant in a pool of data near the code and assembles
//! a PC-relative load from it. The instruction is encoded as its statement
//! is read; the pool is written later, at `.ltorg` or at the end of the
//! section, and until then each use refers to its entry by a label that is
//! defined when the pool is written.
//!
//! Where entries go follows GNU as, whose rules the source that uses pools
//! was written against: one pool per section, collecting every literal since
//! the last `.ltorg`; an entry shared by every use of the same number, or of
//! the same symbol plus the same addend; and at most 1024 entries.
//!
//! The two GNU as ports that have pools lay them out differently, so a
//! backend says which it follows with [`Architecture::literal_pool`]:
//!
//! * [`LiteralPool::ByWidth`], AArch64's: a pool per entry width, written
//!   narrowest first, each run aligned to its own width with zeros rather
//!   than no-ops. A pool there holds four-, eight- and sixteen-byte entries.
//! * [`LiteralPool::Slots`], ARM's: one array of four-byte slots in the
//!   order the literals were asked for, where an eight-byte entry takes two
//!   slots and may need a padding slot in front of it; see
//!   [`Assembler::flush_literal_slots`].
//!
//! [`Architecture::literal_pool`]: crate::arch::Architecture::literal_pool

use crate::arch::{Literal, LiteralPool, LiteralRequest, Request};
use crate::assembler::Assembler;
use crate::expr::ExprRef;
use crate::section::{FragKind, Fragment};
use crate::source::Span;
use crate::symbol::{SymbolId, SymbolValue};

/// The most entries one pool may hold, as in GNU as.
const MAX_ENTRIES: usize = 1024;

/// What makes two literals the same entry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Key {
    /// A number known when the instruction was read.
    Const(i64),
    /// A symbol plus an addend.
    Symbol(SymbolId, i64),
    /// Anything else, which GNU as never shares; the index keeps it apart.
    Unique(usize),
}

struct Entry {
    key: Key,
    value: Literal,
    size: u8,
    labels: Vec<crate::intern::Name>,
    span: Span,
}

/// GNU as's `PADDING_SLOT`, the `X_md` bit marking a slot that is only
/// there to align the eight-byte entry after it, and that a later four-byte
/// entry may take over.
const PADDING_SLOT: u16 = 1 << 8;

/// One four-byte slot of an ARM pool; see
/// [`Assembler::flush_literal_slots`].
struct Slot {
    value: SlotValue,
    /// GNU as's `X_md`: how many bytes the slot emits, in its low byte,
    /// with [`PADDING_SLOT`] above them.
    md: u16,
}

/// Four bytes of a written pool: a place a use may name, and what goes
/// there.
struct Chunk {
    /// The bytes to write, for a number.
    bytes: Vec<u8>,
    /// Or the value to relocate, and how many bytes wide it is.
    value: Option<(u8, ExprRef)>,
    span: Span,
}

/// A slot's expression, as much of it as GNU as compares or emits.
enum SlotValue {
    /// `O_constant`: the number, and whether the source wrote it unsigned,
    /// which GNU as compares as well.
    Word {
        value: i64,
        unsigned: bool,
        span: Span,
    },
    /// Anything else, which only a four-byte entry may hold: the expression
    /// the entry relocates, and the symbol and addend GNU as shares such an
    /// entry by, where the expression has them.
    Other {
        expr: ExprRef,
        key: Option<(SymbolId, i64)>,
        span: Span,
    },
}

impl SlotValue {
    /// Whether a four-byte literal takes this slot: `add_to_lit_pool`'s two
    /// tests, for the same number or for the same symbol plus addend.
    fn shares(&self, new: &SlotValue) -> bool {
        match (self, new) {
            (
                SlotValue::Word {
                    value: a,
                    unsigned: ua,
                    ..
                },
                SlotValue::Word {
                    value: b,
                    unsigned: ub,
                    ..
                },
            ) => a == b && ua == ub,
            (SlotValue::Other { key: Some(a), .. }, SlotValue::Other { key: Some(b), .. }) => {
                a == b
            }
            _ => false,
        }
    }

    /// Whether this slot holds exactly this word, which is what half of an
    /// eight-byte literal is matched against.
    fn is_word(&self, word: i64, unsigned: bool) -> bool {
        match *self {
            SlotValue::Word {
                value, unsigned: u, ..
            } => value == word && u == unsigned,
            SlotValue::Other { .. } => false,
        }
    }

    /// The two words an eight-byte literal is split into, low first, as
    /// `add_to_lit_pool` computes `imm1` and `imm2` from the operand.
    fn halves(&self) -> (i64, i64) {
        match *self {
            SlotValue::Word { value, .. } => {
                let v = value as u64;
                ((v & 0xffff_ffff) as i64, (v >> 32) as i64)
            }
            SlotValue::Other { .. } => (0, 0),
        }
    }

    fn unsigned(&self) -> bool {
        match *self {
            SlotValue::Word { unsigned, .. } => unsigned,
            SlotValue::Other { .. } => true,
        }
    }
}

impl Assembler {
    /// Carries out what a backend asked for while assembling a statement.
    pub(crate) fn run_requests(&mut self, requests: Vec<Request>, span: Span) {
        for r in requests {
            match r {
                Request::AlignZero(align) => {
                    if align > 1 {
                        if self.arch.align_padding_is_code() {
                            self.map_code();
                        } else {
                            self.map_data_frag();
                        }
                        self.cur_section().push(Fragment::new(
                            FragKind::Align {
                                align,
                                fill: vec![0],
                                max_skip: None,
                                pad: 0,
                                nop_state: None,
                            },
                            span,
                        ));
                    }
                }
                Request::RecordAlign(align) => {
                    let s = self.cur_section();
                    s.align = s.align.max(align);
                }
                Request::AlignCode { align, max_skip } => {
                    let state = self
                        .cur_section()
                        .nop_state
                        .clone()
                        .unwrap_or_else(|| self.arch_state.clone());
                    self.map_code_align(&state);
                    self.cur_section().push(Fragment::new(
                        FragKind::Align {
                            align,
                            fill: Vec::new(),
                            max_skip: Some(max_skip),
                            pad: 0,
                            nop_state: Some(state),
                        },
                        span,
                    ));
                }
                Request::Literal(lit) => self.literal_pools.entry(self.cur).or_default().push(lit),
                Request::FlushLiterals => self.flush_literals(span),
                // The words a directive expands to are code like any other,
                // so they mark the section as holding instructions; only
                // the line table is left out, since neither reference
                // records a row for a directive.
                Request::Emit { bytes, code } => {
                    if self.check_nobits(span) {
                        continue;
                    }
                    if code {
                        self.map_code();
                        self.cur_section().has_instructions = true;
                    } else {
                        self.map_data();
                    }
                    self.cur_section().emit_variants(vec![bytes], span);
                }
                // The last word on a tag wins, as it does in GNU as, where
                // each directive overwrites the attribute.
                Request::Attribute { vendor, tag, value } => {
                    self.attr_overrides
                        .retain(|&(v, t, _)| (v, t) != (vendor, tag));
                    self.attr_overrides.push((vendor, tag, value));
                }
                // The statement's own bytes are already in the section's last
                // fragment: the requests run after it is pushed, and only
                // `AlignCode` is carried out in front of it.
                Request::ImplicitIt => {
                    let frag = self.cur_section().next_frag_index().saturating_sub(1);
                    self.implicit_it_at = Some((self.cur, frag));
                }
                Request::ImplicitItMask(bytes) => {
                    if let Some((_, frag)) = self.implicit_it_at.filter(|&(s, _)| s == self.cur)
                        && let Some(f) = self.cur_section().frags.get_mut(frag as usize)
                        && let FragKind::Bytes { variants, .. } = &mut f.kind
                    {
                        for v in variants.iter_mut().filter(|v| v.bytes.len() >= bytes.len()) {
                            v.bytes[..bytes.len()].copy_from_slice(&bytes);
                        }
                    }
                }
                Request::Mark {
                    expr,
                    kind,
                    as_data,
                    within,
                } => {
                    if as_data {
                        self.map_data();
                    }
                    let espan = self.exprs.span(expr);
                    let section = self.cur;
                    let s = self.cur_section();
                    s.emit_fixup(0, expr, kind, espan);
                    // The fixup went into the open data fragment, which is
                    // always the section's last.
                    let frag = s.frags.len() - 1;
                    let offset = s.frags[frag].size() as u32;
                    if within > 0 {
                        self.mark_tests
                            .push((section, frag as u32, offset, within, span));
                    }
                }
            }
        }
    }

    /// Writes every pool that is still open at the end of its section.
    pub(crate) fn flush_all_literals(&mut self) {
        let mut open: Vec<_> = self.literal_pools.keys().copied().collect();
        open.sort();
        let saved = self.cur;
        for id in open {
            self.cur = id;
            self.flush_literals(Span::DUMMY);
        }
        self.cur = saved;
    }

    /// Writes the current section's pool here, and starts a new one.
    pub(crate) fn flush_literals(&mut self, span: Span) {
        let Some(requests) = self.literal_pools.remove(&self.cur) else {
            return;
        };
        match self.arch.literal_pool() {
            LiteralPool::ByWidth => self.flush_literals_by_width(requests, span),
            LiteralPool::Slots => self.flush_literal_slots(requests, span),
        }
    }

    /// Writes a pool laid out as GNU as's AArch64 port lays one out: a run
    /// of entries per width, narrowest first; see [`LiteralPool::ByWidth`].
    fn flush_literals_by_width(&mut self, requests: Vec<LiteralRequest>, span: Span) {
        let mut entries: Vec<Entry> = Vec::new();
        for (i, r) in requests.into_iter().enumerate() {
            let key = self.literal_key(&r, i);
            if let Some(e) = entries
                .iter_mut()
                .find(|e| e.key == key && e.size == r.size)
            {
                e.labels.push(r.label);
                continue;
            }
            if entries.len() == MAX_ENTRIES {
                self.diags.error(
                    r.span,
                    format!("literal pool overflow: a pool holds at most {MAX_ENTRIES} entries"),
                );
                continue;
            }
            entries.push(Entry {
                key,
                value: r.value,
                size: r.size,
                labels: vec![r.label],
                span: r.span,
            });
        }
        if entries.is_empty() {
            return;
        }
        // GNU as keeps a pool per entry width and writes them narrowest
        // first, each aligned with zeros even in code, and marks the padding
        // and the entries as data.
        entries.sort_by_key(|e| e.size);
        let section = self.cur;
        let mut written = 0u8;
        for e in entries {
            if e.size != written {
                written = e.size;
                let align = u64::from(e.size);
                self.run_requests(vec![Request::AlignZero(align)], span);
                let s = self.cur_section();
                s.align = s.align.max(align);
                if let Some((_, _, data)) =
                    crate::mapping::mapping_names(self.arch.as_ref(), &self.arch_state)
                {
                    self.map_transition(data, true);
                }
            }
            self.cur_section().seal();
            let frag = self.cur_section().next_frag_index();
            for label in e.labels {
                let id = self.symbols.intern(label, e.span);
                let sym = self.symbols.get_mut(id);
                sym.value = SymbolValue::Label { section, frag };
                sym.def_span = e.span;
                self.symbols.mark_defined(id);
            }
            match e.value {
                Literal::Const(v) => {
                    let endian = self.arch.endian();
                    let mut bytes = endian.bytes(v as u64, usize::from(e.size).min(8));
                    if usize::from(e.size) > bytes.len() {
                        // A sixteen-byte entry holds the value sign-extended,
                        // as GNU as writes it: `ldr q0, =-1` is sixteen 0xff
                        // bytes.
                        let fill = vec![u8::from(v < 0) * 0xff; usize::from(e.size) - bytes.len()];
                        match endian {
                            crate::arch::Endian::Little => bytes.extend(fill),
                            crate::arch::Endian::Big => {
                                bytes.splice(0..0, fill);
                            }
                        }
                    }
                    self.cur_section().emit_bytes(&bytes, e.span);
                }
                Literal::Expr(x) => self.emit_value(e.size, x, e.span),
            }
        }
    }

    /// Writes a pool laid out as GNU as's ARM port lays one out; see
    /// [`LiteralPool::Slots`].
    ///
    /// `add_to_lit_pool` in `gas/config/tc-arm.c` keeps the pool as an array
    /// of four-byte slots and walks it from the start for every literal, so
    /// this walks the same array the same way:
    ///
    /// * a four-byte literal takes the first slot holding the same number
    ///   (the same `X_add_number` and `X_unsigned`), or the same symbol and
    ///   addend; failing that, the first *padding* slot; failing that, a new
    ///   slot at the end;
    /// * an eight-byte literal has to be a number, and takes two slots
    ///   starting at an eight-aligned offset: it takes a pair already
    ///   holding its two halves, or appends, first appending a zero padding
    ///   slot if the end of the pool is not eight-aligned. From then on the
    ///   pool itself is aligned to eight — and stays so for the rest of the
    ///   section, since `pool->alignment` outlives the pool the `.ltorg`
    ///   emptied;
    /// * the offset of a use is four times the index of its slot, whatever
    ///   the slots before it emit. GNU as can write a slot that emits eight
    ///   bytes (see below), and then the two disagree, which is where a load
    ///   of the wrong bytes comes from. This follows GNU as.
    fn flush_literal_slots(&mut self, requests: Vec<LiteralRequest>, span: Span) {
        // GNU as's `pool->alignment`, as the byte alignment it means.
        let mut align = self.literal_pool_align.get(&self.cur).copied().unwrap_or(4);
        let endian = self.arch.endian();
        let mut slots: Vec<Slot> = Vec::new();
        // Which slot each use points at, as its index: the labels of every
        // instruction that asked for a literal, in the order they asked.
        let mut uses: Vec<(usize, crate::intern::Name, Span)> = Vec::new();
        for r in requests {
            let new = self.slot_value(&r);
            // GNU as writes the half that comes first in memory first, so
            // on a big-endian target the two are swapped.
            let (imm1, imm2) = match (new.halves(), endian) {
                ((lo, hi), crate::arch::Endian::Little) => (lo, hi),
                ((lo, hi), crate::arch::Endian::Big) => (hi, lo),
            };
            let unsigned = new.unsigned();
            // The search of `add_to_lit_pool`, including where it leaves
            // `padding_slot_p`: the eight-byte match breaks out before the
            // line that assigns it, so a match right after a padding slot
            // is taken for the padding slot itself.
            let mut entry = 0;
            let mut pool_size = 0usize;
            let mut padding_slot_p = false;
            while entry < slots.len() {
                if r.size == 4 {
                    if slots[entry].md == 4 && slots[entry].value.shares(&new) {
                        break;
                    }
                } else if r.size == 8
                    && pool_size.is_multiple_of(8)
                    && entry + 1 != slots.len()
                    && slots[entry].value.is_word(imm1, unsigned)
                    && slots[entry + 1].value.is_word(imm2, unsigned)
                {
                    break;
                }
                padding_slot_p = slots[entry].md & PADDING_SLOT != 0;
                if padding_slot_p && r.size == 4 {
                    break;
                }
                pool_size += 4;
                entry += 1;
            }
            if entry == slots.len() {
                let needed = if r.size == 8 {
                    if pool_size.is_multiple_of(8) { 2 } else { 3 }
                } else {
                    1
                };
                let refuse = if entry + needed > MAX_ENTRIES {
                    Some(format!(
                        "literal pool overflow: a pool holds at most {MAX_ENTRIES} slots"
                    ))
                } else if needed > 1 && !matches!(new, SlotValue::Word { .. }) {
                    // `add_to_lit_pool`'s "invalid type for literal pool",
                    // which the ARM backend reports for itself: a pair of
                    // slots holds two numbers and nothing else.
                    Some("an entry this wide must be a number".to_string())
                } else {
                    None
                };
                if let Some(why) = refuse {
                    self.diags.error(r.span, why);
                    // The load keeps the slot before it, so that a pool it
                    // cannot have a slot in does not also leave its label
                    // undefined and report that of every load after it.
                    if entry > 0 {
                        uses.push((entry - 1, r.label, r.span));
                    }
                    continue;
                }
                if r.size == 8 {
                    if !pool_size.is_multiple_of(8) {
                        slots.push(Slot {
                            value: SlotValue::Word {
                                value: 0,
                                unsigned,
                                span: r.span,
                            },
                            md: PADDING_SLOT | 4,
                        });
                        // The entry moves on with the slot, which is what
                        // keeps GNU as's `pool_size` and four times the
                        // index the same number.
                        entry += 1;
                    }
                    for value in [imm1, imm2] {
                        slots.push(Slot {
                            value: SlotValue::Word {
                                value,
                                unsigned,
                                span: r.span,
                            },
                            md: 4,
                        });
                    }
                    align = 8;
                } else {
                    slots.push(Slot { value: new, md: 4 });
                }
            } else if padding_slot_p {
                // The literal took over a padding slot -- or GNU as thinks
                // it did, and rewrites the slot it really matched, which for
                // an eight-byte literal makes that slot emit all eight
                // bytes and leaves the second half of the pair behind as a
                // slot of its own.
                slots[entry] = Slot {
                    value: new,
                    md: u16::from(r.size),
                };
            }
            uses.push((entry, r.label, r.span));
        }
        if slots.is_empty() {
            return;
        }
        // `s_ltorg`: align the pool, mark it as data, and write the slots.
        // `record_alignment (now_seg, 2)` raises the section's alignment to
        // four whatever the pool itself needs.
        self.literal_pool_align.insert(self.cur, align);
        self.run_requests(vec![Request::AlignZero(align)], span);
        let s = self.cur_section();
        s.align = s.align.max(4);
        if let Some((_, _, data)) =
            crate::mapping::mapping_names(self.arch.as_ref(), &self.arch_state)
        {
            self.map_transition(data, true);
        }
        // Each four bytes of the pool gets a fragment of its own, so that a
        // use can name the one four times its slot index whatever the slots
        // before it wrote.
        let mut chunks: Vec<Chunk> = Vec::new();
        for slot in slots {
            let size = (slot.md & 0xff) as u8;
            match slot.value {
                SlotValue::Word { value, span, .. } => {
                    let bytes = endian.bytes(value as u64, usize::from(size));
                    for chunk in bytes.chunks(4) {
                        chunks.push(Chunk {
                            bytes: chunk.to_vec(),
                            value: None,
                            span,
                        });
                    }
                }
                SlotValue::Other { expr, span, .. } => {
                    chunks.push(Chunk {
                        bytes: Vec::new(),
                        value: Some((size, expr)),
                        span,
                    });
                    // A relocated entry is four bytes wide in every pool
                    // there is; any rest of one is a place a use may name,
                    // holding no bytes of its own.
                    for _ in 1..size / 4 {
                        chunks.push(Chunk {
                            bytes: Vec::new(),
                            value: None,
                            span,
                        });
                    }
                }
            }
        }
        let section = self.cur;
        for (i, chunk) in chunks.into_iter().enumerate() {
            self.cur_section().seal();
            let frag = self.cur_section().next_frag_index();
            for (_, label, span) in uses.iter().filter(|(slot, _, _)| *slot == i) {
                let id = self.symbols.intern(*label, *span);
                let sym = self.symbols.get_mut(id);
                sym.value = SymbolValue::Label { section, frag };
                sym.def_span = *span;
                self.symbols.mark_defined(id);
            }
            match chunk.value {
                Some((size, expr)) => self.emit_value(size, expr, chunk.span),
                None => self.cur_section().emit_bytes(&chunk.bytes, chunk.span),
            }
        }
    }

    /// One literal as GNU as's ARM pool would hold it.
    fn slot_value(&mut self, r: &LiteralRequest) -> SlotValue {
        match r.value {
            Literal::Const(value) => SlotValue::Word {
                value,
                unsigned: r.unsigned,
                span: r.span,
            },
            Literal::Expr(expr) => SlotValue::Other {
                expr,
                key: match self.eval(expr) {
                    Ok(v) if v.minus.is_none() => v.plus.map(|p| (p, v.addend)),
                    _ => None,
                },
                span: r.span,
            },
        }
    }

    /// Which entry a literal shares, by GNU as's rule: the same number, or
    /// the same symbol and addend.
    fn literal_key(&mut self, r: &LiteralRequest, index: usize) -> Key {
        match r.value {
            Literal::Const(v) => Key::Const(v),
            Literal::Expr(e) => match self.eval(e) {
                Ok(v) if v.minus.is_none() => match v.plus {
                    Some(p) => Key::Symbol(p, v.addend),
                    None => Key::Unique(index),
                },
                _ => Key::Unique(index),
            },
        }
    }
}
