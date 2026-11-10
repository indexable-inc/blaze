//! The verifier: what makes columns a LIR function.
//!
//! [`structure`] checks what every reader relies on to index without going out of bounds:
//! each column's length, each tag, each table row's ranges, each operand's words, each index
//! into a table, and that `extra` holds the operands in instruction order and nothing else, so
//! one function has one encoding. A record read from bytes ([`crate::persist::read`]) is
//! checked this far.
//!
//! [`verify`] checks that, then what makes the function mean something: each instruction's
//! flags against its class, its operands' and result's types against [`Op::info`] and the
//! operation's rule, each block's parameters at its head and one terminator at its end, each
//! edge's arguments against its block's parameters, landing blocks reached by unwind edges
//! alone, and every use dominated by its value. The builder runs it in debug builds, and in
//! release builds behind a check mode ([`crate::Builder::finish_checked`]).

use std::fmt;

use crate::{
    flags::Ordering,
    func::{
        AsmOperandKind, AsmPieceRec, Block, BlockKind, Conv, Ext, MemAttrsRec, NONE, ParamKind,
        ParamRec, RefKind, SCOPE_IN, SigId, SigRec, SlotKind, Value, View,
    },
    op::{End, Field, Layout, Op, Res, Values, WalkError, Word, walk},
    ty::{Form, Scalar, Ty},
};

/// Where a rule was broken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum At {
    /// The function as a whole.
    Func,
    /// An instruction, with its operation's name (`?` for a byte that names none).
    Inst { inst: u32, op: &'static str },
    /// A block, in layout order.
    Block(u32),
    /// A row of a table, or an item of a column.
    Row { table: &'static str, row: u32 },
}

/// A rule of LIR a function breaks, and where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifyError {
    pub at: At,
    pub rule: &'static str,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            At::Func => write!(f, "the function: {}", self.rule),
            At::Inst { inst, op } => write!(f, "v{inst} (`{op}`): {}", self.rule),
            At::Block(block) => write!(f, "block{block}: {}", self.rule),
            At::Row { table, row } => write!(f, "row {row} of `{table}`: {}", self.rule),
        }
    }
}

impl std::error::Error for VerifyError {}

type Result<T = ()> = std::result::Result<T, VerifyError>;

fn func(rule: &'static str) -> VerifyError {
    VerifyError { at: At::Func, rule }
}

fn row(table: &'static str, row: usize, rule: &'static str) -> VerifyError {
    VerifyError { at: At::Row { table, row: row as u32 }, rule }
}

fn inst(view: &View<'_>, at: usize, rule: &'static str) -> VerifyError {
    let op = view.ops.get(at).and_then(|&it| Op::from_byte(it)).map_or("?", Op::name);
    VerifyError { at: At::Inst { inst: at as u32, op }, rule }
}

fn block(at: usize, rule: &'static str) -> VerifyError {
    VerifyError { at: At::Block(at as u32), rule }
}

/// Whether `first..first + len` lies in a column of `of` items.
fn within(first: u32, len: u32, of: usize) -> bool {
    (first as u64 + len as u64) <= of as u64
}

/// Whether a 0-or-1-plus-row field names nothing or a row of a table of `rows`.
fn optional(field: u32, rows: usize) -> bool {
    field as usize <= rows
}

/// Checks `view` all the way: [`structure`], then what it means.
pub fn verify(view: View<'_>) -> Result {
    structure(view)?;
    semantics(&view)
}

/// Checks what every reader of `view` relies on: lengths, tags, ranges and indices.
pub fn structure(view: View<'_>) -> Result {
    let n = view.ops.len();
    if !matches!(view.head.ptr_bits, 32 | 64) {
        return Err(func("a pointer width that is neither 32 nor 64"));
    }
    if view.head.attrs & !crate::func::FnAttrs::ALL != 0 {
        return Err(func("function attributes past the known ones"));
    }
    for (name, len) in [
        ("flags", view.flags.len()),
        ("tys", view.tys.len()),
        ("data", view.data.len()),
        ("attrs", view.attrs.len()),
        ("locs", view.locs.len()),
    ] {
        if len != n {
            return Err(row(name, len, "a per-instruction column of another length than `ops`"));
        }
    }
    if u32::try_from(n).is_err() || u32::try_from(view.extra.len()).is_err() {
        return Err(func("more instructions or operand words than a u32 counts"));
    }
    structure_blocks(&view)?;
    structure_tables(&view)?;
    structure_debug(&view)?;
    // Each instruction: its tag, its type, its operands, its attributes and location.
    let mut next_extra = 0usize;
    for at in 0..n {
        let Some(op) = Op::from_byte(view.ops[at]) else {
            return Err(inst(&view, at, "a byte that names no operation"));
        };
        let ty = Ty(view.tys[at]);
        check_ty(&view, ty).map_err(|rule| inst(&view, at, rule))?;
        let info = op.info();
        let data = view.data[at];
        let mut visit = |word: Word| index(&view, word);
        match info.layout {
            Layout::None => {
                if data != 0 {
                    return Err(inst(&view, at, "a `data` word where its operation has no operand"));
                }
            }
            Layout::Data(field) => {
                let word = match field {
                    Field::Value => Word::Value(data),
                    Field::Block => Word::Block(data),
                    Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm
                    | Field::Target
                    | Field::Values
                    | Field::Blocks
                    | Field::Mask
                    | Field::Const
                    | Field::Cases => Word::Of(field, data),
                };
                visit(word).map_err(|rule| inst(&view, at, rule))?;
            }
            Layout::Extra(fields) => {
                if data as usize != next_extra {
                    return Err(inst(&view, at, "operands that do not follow the instruction before's in `extra`"));
                }
                let words = &view.extra[next_extra..];
                let taken = walk(words, fields, ty, view.tys, &mut visit).map_err(|error| {
                    let rule = match error {
                        WalkError::Short => "operands past the end of `extra`",
                        WalkError::NoScrutinee => "cases with no scrutinee before them",
                        WalkError::Visit(rule) => rule,
                    };
                    inst(&view, at, rule)
                })?;
                next_extra += taken;
            }
        }
        let attrs = view.attrs[at];
        if !optional(attrs, view.mem_attrs.len()) {
            return Err(inst(&view, at, "an `attrs` word past `mem_attrs`"));
        }
        if attrs != 0 && matches!(info.attrs, crate::op::AttrUse::None) {
            return Err(inst(&view, at, "memory attributes on an operation that takes none"));
        }
        if !optional(view.locs[at], view.spans.len()) {
            return Err(inst(&view, at, "a location past `spans`"));
        }
    }
    if next_extra != view.extra.len() {
        return Err(func("words in `extra` no instruction takes"));
    }
    Ok(())
}

/// Whether `ty` is a type of the encoding whose compound row exists.
fn check_ty(view: &View<'_>, ty: Ty) -> std::result::Result<(), &'static str> {
    if !ty.is_valid() {
        return Err("a type of no known encoding");
    }
    match ty.compound_row() {
        Some(row) if row as usize >= view.compounds.len() => Err("a compound type past `compounds`"),
        Some(_) | None => Ok(()),
    }
}

/// Whether one word of an operand names something that exists.
fn index(view: &View<'_>, word: Word) -> std::result::Result<(), &'static str> {
    let ok = match word {
        Word::Value(value) => (value as usize) < view.ops.len(),
        Word::Block(block) => (block as usize) < view.block_starts.len(),
        Word::Of(field, word) => match field {
            Field::IntCond => crate::flags::IntCond::from_word(word).is_some(),
            Field::FloatCond => crate::flags::FloatCond::from_word(word).is_some(),
            Field::Rmw => crate::flags::AtomicRmw::from_word(word).is_some(),
            Field::Ref => (word as usize) < view.refs.len(),
            Field::Sig => (word as usize) < view.sigs.len(),
            Field::Slot => (word as usize) < view.slots.len(),
            Field::Intrinsic => (word as usize) < view.intrinsics.len(),
            Field::Asm => (word as usize) < view.asms.len(),
            Field::Imm
            | Field::Offset
            | Field::Mask
            | Field::Const
            | Field::Cases
            | Field::Value
            | Field::Block
            | Field::Target
            | Field::Values
            | Field::Blocks => true,
        },
    };
    match ok {
        true => Ok(()),
        false => Err(match word {
            Word::Value(_) => "a value past the last instruction",
            Word::Block(_) => "a block past the last block",
            Word::Of(..) => "an operand that names no row, condition or operation",
        }),
    }
}

fn structure_blocks(view: &View<'_>) -> Result {
    let n = view.ops.len();
    if view.block_starts.len() != view.block_kinds.len() {
        return Err(row("block_kinds", view.block_kinds.len(), "a block column of another length than `block_starts`"));
    }
    if n == 0 || view.block_starts.is_empty() {
        return Err(func("no instructions or no blocks: a function has an entry block"));
    }
    if view.block_starts[0] != 0 {
        return Err(block(0, "an entry block that does not start at the first instruction"));
    }
    for (b, &start) in view.block_starts.iter().enumerate() {
        if start as usize >= n {
            return Err(block(b, "a block that starts past the last instruction"));
        }
        if b > 0 && start <= view.block_starts[b - 1] {
            return Err(block(b, "a block that does not start after the one before it: blocks are in layout order and none is empty"));
        }
        if BlockKind::from_byte(view.block_kinds[b]).is_none() {
            return Err(block(b, "a block kind of no known kind"));
        }
    }
    Ok(())
}

fn structure_tables(view: &View<'_>) -> Result {
    let ptr_bytes = view.head.ptr_bits / 8;
    if view.head.sig as usize >= view.sigs.len() {
        return Err(func("its own signature past `sigs`"));
    }
    for (r, rec) in view.compounds.iter().enumerate() {
        if !within(rec.first, rec.len, view.compound_tys.len()) {
            return Err(row("compounds", r, "members past `compound_tys`"));
        }
        if rec.len == 0 {
            return Err(row("compounds", r, "a compound of no members"));
        }
    }
    for (r, &member) in view.compound_tys.iter().enumerate() {
        let ty = Ty(member);
        if !ty.is_valid() || ty.is_none() || matches!(ty.form(), Form::Compound) {
            return Err(row("compound_tys", r, "a member that is not a scalar or vector"));
        }
    }
    for (r, rec) in view.sigs.iter().enumerate() {
        if !within(rec.first, rec.params as u32 + rec.rets as u32, view.sig_params.len()) {
            return Err(row("sigs", r, "parameters past `sig_params`"));
        }
        if Conv::from_byte(rec.conv).is_none() || rec.bits & !SigRec::VARIADIC != 0 || rec.pad != [0; 2] {
            return Err(row("sigs", r, "a convention, bit or padding of no known meaning"));
        }
    }
    for (r, rec) in view.sig_params.iter().enumerate() {
        let ty = Ty(rec.ty);
        if check_ty(view, ty).is_err() || ty.is_none() {
            return Err(row("sig_params", r, "a type that is not one"));
        }
        if Ext::from_byte(rec.ext).is_none() || ParamKind::from_byte(rec.kind).is_none() {
            return Err(row("sig_params", r, "an extension or kind of no known meaning"));
        }
        if !optional(rec.facts, view.param_facts.len()) {
            return Err(row("sig_params", r, "facts past `param_facts`"));
        }
    }
    for (r, rec) in view.param_facts.iter().enumerate() {
        if !optional(rec.range, view.ranges.len()) || rec.pad != [0; 6] || rec.align >= 64 {
            return Err(row("param_facts", r, "a range past `ranges`, padding or an alignment of no meaning"));
        }
        if *rec == crate::func::ParamFactsRec::default() {
            return Err(row("param_facts", r, "facts that claim nothing, which are written as none"));
        }
    }
    let mem_bits = MemAttrsRec::NAMED.iter().fold(0, |all, it| all | it.0);
    for (r, rec) in view.mem_attrs.iter().enumerate() {
        if !optional(rec.range, view.ranges.len()) || rec.bits & !mem_bits != 0 {
            return Err(row("mem_attrs", r, "a range past `ranges` or a bit of no meaning"));
        }
        if rec.align >= 64 || rec.src_align >= 64 || rec.pointee_align >= 64 {
            return Err(row("mem_attrs", r, "an alignment past 2^63"));
        }
        if *rec == MemAttrsRec::default() {
            return Err(row("mem_attrs", r, "attributes that claim nothing, which are written as none"));
        }
    }
    for (r, rec) in view.refs.iter().enumerate() {
        match RefKind::from_word(rec.kind) {
            None => return Err(row("refs", r, "a kind of no known meaning")),
            Some(RefKind::Data) if rec.place as usize >= view.datas.len() => {
                return Err(row("refs", r, "data past `datas`"));
            }
            Some(RefKind::Data | RefKind::Func | RefKind::Static | RefKind::Tls) => {}
        }
    }
    for (r, rec) in view.datas.iter().enumerate() {
        if !within(rec.bytes_first, rec.bytes_len, view.data_bytes.len())
            || !within(rec.relocs_first, rec.relocs_len, view.data_relocs.len())
        {
            return Err(row("datas", r, "bytes or addresses past their columns"));
        }
        if rec.align >= 32 {
            return Err(row("datas", r, "an alignment past 2^31"));
        }
        let relocs = &view.data_relocs[rec.relocs_first as usize..][..rec.relocs_len as usize];
        for reloc in relocs {
            if reloc.target as usize >= view.refs.len()
                || reloc.offset as u64 + ptr_bytes as u64 > rec.bytes_len as u64
            {
                return Err(row("datas", r, "an address past its data or naming no reference"));
            }
        }
    }
    for (r, rec) in view.intrinsics.iter().enumerate() {
        if !within(rec.name_first, rec.name_len, view.names.len()) || rec.sig as usize >= view.sigs.len() {
            return Err(row("intrinsics", r, "a name past `names` or a signature past `sigs`"));
        }
        if std::str::from_utf8(&view.names[rec.name_first as usize..][..rec.name_len as usize]).is_err() {
            return Err(row("intrinsics", r, "a name that is not UTF-8"));
        }
    }
    for (r, rec) in view.slots.iter().enumerate() {
        if SlotKind::from_byte(rec.kind).is_none() || rec.pad != [0; 2] || rec.align >= 32 {
            return Err(row("slots", r, "a kind, padding or alignment of no meaning"));
        }
    }
    for (r, rec) in view.asms.iter().enumerate() {
        if !within(rec.pieces_first, rec.pieces_len, view.asm_pieces.len())
            || !within(rec.operands_first, rec.operands_len, view.asm_operands.len())
            || !within(rec.clobbers_first, rec.clobbers_len, view.asm_text.len())
        {
            return Err(row("asms", r, "pieces, operands or clobbers past their columns"));
        }
        let options = crate::func::AsmRec::NAMED.iter().fold(0, |all, it| all | it.0);
        if rec.options & !options != 0 {
            return Err(row("asms", r, "an option of no known meaning"));
        }
        if std::str::from_utf8(view.asm_str(rec.clobbers_first, rec.clobbers_len)).is_err() {
            return Err(row("asms", r, "clobbers that are not UTF-8"));
        }
        let pieces = &view.asm_pieces[rec.pieces_first as usize..][..rec.pieces_len as usize];
        let mut text_before = false;
        for piece in pieces {
            match piece.kind {
                AsmPieceRec::TEXT => {
                    if !within(piece.a, piece.b, view.asm_text.len())
                        || std::str::from_utf8(view.asm_str(piece.a, piece.b)).is_err()
                    {
                        return Err(row("asms", r, "template text past `asm_text` or not UTF-8"));
                    }
                    if piece.b == 0 || text_before {
                        return Err(row("asms", r, "an empty text piece or two in a row, which are written as one"));
                    }
                    text_before = true;
                }
                AsmPieceRec::OPERAND => {
                    let modifier = char::from_u32(piece.b).filter(|it| piece.b == 0 || it.is_ascii_alphanumeric());
                    if piece.a >= rec.operands_len || modifier.is_none() {
                        return Err(row("asms", r, "a template operand past its operands or a modifier that is not a letter or digit"));
                    }
                    text_before = false;
                }
                _ => return Err(row("asms", r, "a template piece of no known kind")),
            }
        }
        let operands = &view.asm_operands[rec.operands_first as usize..][..rec.operands_len as usize];
        for operand in operands {
            let Some(kind) = AsmOperandKind::from_byte(operand.kind) else {
                return Err(row("asms", r, "an operand of no known kind"));
            };
            if !within(operand.reg_first, operand.reg_len, view.asm_text.len())
                || std::str::from_utf8(view.asm_str(operand.reg_first, operand.reg_len)).is_err()
                || operand.class > 1
                || operand.pad != [0; 2]
            {
                return Err(row("asms", r, "an operand's register past `asm_text`, or its class or padding of no meaning"));
            }
            if matches!(kind, AsmOperandKind::Sym) && operand.aux as usize >= view.refs.len() {
                return Err(row("asms", r, "a `sym` operand naming no reference"));
            }
        }
    }
    if std::str::from_utf8(view.features).is_err() {
        return Err(func("target features that are not UTF-8"));
    }
    canonical(view)
}

/// Whether the ranges `(first, len)` follow one another from 0 and end at `total`.
fn tiles(ranges: impl Iterator<Item = (u32, u32)>, total: usize) -> bool {
    let mut at = 0u64;
    for (first, len) in ranges {
        if first as u64 != at {
            return false;
        }
        at += len as u64;
    }
    at == total as u64
}

/// Each list table's rows lie one after another in their columns, in row order, and fill
/// them: so one function has one encoding, and equal functions are equal bytes.
fn canonical(view: &View<'_>) -> Result {
    let checks: [(&'static str, bool); 8] = [
        ("compounds", tiles(view.compounds.iter().map(|it| (it.first, it.len)), view.compound_tys.len())),
        (
            "sigs",
            tiles(
                view.sigs.iter().map(|it| (it.first, it.params as u32 + it.rets as u32)),
                view.sig_params.len(),
            ),
        ),
        ("datas", tiles(view.datas.iter().map(|it| (it.bytes_first, it.bytes_len)), view.data_bytes.len())),
        ("datas", tiles(view.datas.iter().map(|it| (it.relocs_first, it.relocs_len)), view.data_relocs.len())),
        ("intrinsics", tiles(view.intrinsics.iter().map(|it| (it.name_first, it.name_len)), view.names.len())),
        ("asms", tiles(view.asms.iter().map(|it| (it.pieces_first, it.pieces_len)), view.asm_pieces.len())),
        ("asms", tiles(view.asms.iter().map(|it| (it.operands_first, it.operands_len)), view.asm_operands.len())),
        ("asms", {
            // Each block's text: its template's text pieces, its operands' registers, then
            // its clobbers.
            let text = view.asms.iter().flat_map(|rec| {
                let pieces = view.asm_pieces[rec.pieces_first as usize..][..rec.pieces_len as usize]
                    .iter()
                    .filter(|it| it.kind == AsmPieceRec::TEXT)
                    .map(|it| (it.a, it.b));
                let regs = view.asm_operands[rec.operands_first as usize..][..rec.operands_len as usize]
                    .iter()
                    .map(|it| (it.reg_first, it.reg_len));
                pieces.chain(regs).chain(std::iter::once((rec.clobbers_first, rec.clobbers_len)))
            });
            tiles(text, view.asm_text.len())
        }),
    ];
    match checks.iter().find(|it| !it.1) {
        Some(&(table, _)) => Err(row(table, 0, "rows that do not follow one another in their columns, in row order")),
        None => Ok(()),
    }
}

fn structure_debug(view: &View<'_>) -> Result {
    for (name, column) in [("span_scopes", view.span_scopes), ("span_inlined", view.span_inlined)] {
        if !column.is_empty() && column.len() != view.spans.len() {
            return Err(row(name, column.len(), "a per-span column of another length than `spans`"));
        }
    }
    for (r, &scope) in view.span_scopes.iter().enumerate() {
        if scope as u64 >= SCOPE_IN as u64 + view.scope_parents.len() as u64 {
            return Err(row("span_scopes", r, "a scope past `scope_parents`"));
        }
    }
    for (r, &inlined) in view.span_inlined.iter().enumerate() {
        if inlined != NONE && inlined as usize >= view.inlined.len() {
            return Err(row("span_inlined", r, "an inlined body past `inlined`"));
        }
    }
    for (r, &parent) in view.scope_parents.iter().enumerate() {
        if parent != NONE && parent as usize >= r {
            return Err(row("scope_parents", r, "a parent that does not come before it"));
        }
    }
    for (r, rec) in view.inlined.iter().enumerate() {
        if rec.parent != NONE && rec.parent as usize >= r {
            return Err(row("inlined", r, "a parent that does not come before it"));
        }
    }
    for (r, rec) in view.variables.iter().enumerate() {
        if rec.slot as usize >= view.slots.len()
            || (rec.scope != NONE && rec.scope as usize >= view.scope_parents.len())
            || rec.param > 1
        {
            return Err(row("variables", r, "a slot or scope past its table, or a parameter flag of no meaning"));
        }
    }
    Ok(())
}

/// An edge from a block's terminator.
#[derive(Clone, Copy, Debug)]
struct Edge<'a> {
    to: Block,
    kind: EdgeKind<'a>,
}

#[derive(Clone, Copy, Debug)]
enum EdgeKind<'a> {
    /// A jump, a branch, a switch's case: with arguments.
    Plain(Values<'a>),
    /// An `invoke`'s return or an `asm_br`'s fall-through: the instruction's value is
    /// defined there.
    Normal,
    /// An `invoke`'s unwind edge: the exception is the landing block's parameter.
    Unwind,
    /// An `asm_br`'s label.
    Label,
}

/// Calls `f` with every edge from the terminator `at`.
fn edges<'a>(view: &View<'a>, at: usize, mut f: impl FnMut(Edge<'a>)) {
    let mut operands = view.operands(at);
    let op = view.op(at);
    if op == Op::Jump {
        let target = operands.target();
        f(Edge { to: target.block, kind: EdgeKind::Plain(target.args) });
    } else if op == Op::Br {
        operands.value();
        for _ in 0..2 {
            let target = operands.target();
            f(Edge { to: target.block, kind: EdgeKind::Plain(target.args) });
        }
    } else if op == Op::Switch {
        let scrutinee = operands.value();
        let default = operands.target();
        f(Edge { to: default.block, kind: EdgeKind::Plain(default.args) });
        for (_, target) in operands.cases(scrutinee) {
            f(Edge { to: target.block, kind: EdgeKind::Plain(target.args) });
        }
    } else if matches!(op, Op::Invoke | Op::InvokeIndirect) {
        operands.word();
        operands.sig();
        f(Edge { to: operands.block(), kind: EdgeKind::Normal });
        f(Edge { to: operands.block(), kind: EdgeKind::Unwind });
    } else if op == Op::AsmBr {
        operands.asm();
        f(Edge { to: operands.block(), kind: EdgeKind::Normal });
        for &label in operands.blocks() {
            f(Edge { to: Block(label), kind: EdgeKind::Label });
        }
    }
}

/// The block of each instruction, by layout.
fn block_of(view: &View<'_>) -> Vec<u32> {
    // alloc: the verifier's scratch, one word per instruction, for one function
    let mut of = vec![0u32; view.len()];
    for b in 0..view.blocks() {
        for at in view.block(Block(b as u32)) {
            of[at] = b as u32;
        }
    }
    of
}

/// What [`semantics`] works from: each block's successors and predecessors, and dominators.
struct Cfg {
    /// Per block, its immediate dominator; [`NONE`] for an unreachable block, itself for the
    /// entry.
    idom: Vec<u32>,
    /// Per block, how many edges lead to it.
    preds: Vec<u32>,
}

impl Cfg {
    fn of(view: &View<'_>) -> Cfg {
        let blocks = view.blocks();
        // alloc: the verifier's scratch, a few words per block, for one function
        let mut succs: Vec<Vec<u32>> = vec![Vec::new(); blocks];
        // alloc: as above
        let mut preds_of: Vec<Vec<u32>> = vec![Vec::new(); blocks];
        // alloc: as above
        let mut preds = vec![0u32; blocks];
        for b in 0..blocks {
            edges(view, view.terminator(Block(b as u32)), |edge| {
                succs[b].push(edge.to.0);
                preds_of[edge.to.index()].push(b as u32);
                preds[edge.to.index()] += 1;
            });
        }
        // Reverse postorder from the entry, by an explicit stack.
        // alloc: as above
        let mut post = Vec::with_capacity(blocks);
        // alloc: as above
        let mut seen = vec![false; blocks];
        // alloc: as above
        let mut stack = vec![(0u32, 0usize)];
        seen[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            if let Some(&s) = succs[b as usize].get(next) {
                top.1 += 1;
                if !seen[s as usize] {
                    seen[s as usize] = true;
                    stack.push((s, 0));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
        // alloc: as above
        let mut order = vec![NONE; blocks];
        for (i, &b) in post.iter().rev().enumerate() {
            order[b as usize] = i as u32;
        }
        // Cooper, Harvey and Kennedy's iteration, bounded: it settles in at most as many
        // rounds as the graph is deep, so twice the blocks is a bound it never reaches.
        // alloc: as above
        let mut idom = vec![NONE; blocks];
        idom[0] = 0;
        let rpo: Vec<u32> = post.iter().rev().copied().collect(); // alloc: as above
        for _ in 0..2 * blocks + 2 {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let mut new = NONE;
                for &p in &preds_of[b as usize] {
                    if idom[p as usize] == NONE {
                        continue;
                    }
                    new = match new {
                        NONE => p,
                        _ => intersect(&idom, &order, p, new),
                    };
                }
                if new != idom[b as usize] {
                    idom[b as usize] = new;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        Cfg { idom, preds }
    }

    fn reachable(&self, b: u32) -> bool {
        self.idom[b as usize] != NONE
    }

    /// Whether `a` dominates `b`, both reachable.
    fn dominates(&self, a: u32, mut b: u32) -> bool {
        // Up the dominator tree from `b`, at most as many steps as there are blocks.
        for _ in 0..self.idom.len() {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = self.idom[b as usize];
        }
        false
    }
}

fn intersect(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    // Each step moves one finger up the tree; the bound is the tree's size twice.
    for _ in 0..2 * idom.len() + 2 {
        if a == b {
            return a;
        }
        while order[a as usize] > order[b as usize] {
            a = idom[a as usize];
        }
        while order[b as usize] > order[a as usize] {
            b = idom[b as usize];
        }
    }
    a
}

/// Checks what `view`, whose [`structure`] holds, means.
fn semantics(view: &View<'_>) -> Result {
    let ptr_int = Ty::int(view.head.ptr_bits).unwrap_or(Ty::I64);
    let own = view.sig(SigId(view.head.sig));
    // Blocks: parameters at the head, one terminator at the end; landing blocks' one
    // parameter.
    for b in 0..view.blocks() {
        let range = view.block(Block(b as u32));
        let params = view.params(Block(b as u32));
        let kind = view.block_kind(Block(b as u32));
        for at in range.clone() {
            let op = view.op(at);
            if op == Op::Param && at >= params.end {
                return Err(inst(view, at, "a parameter after an instruction of its block"));
            }
            let last = at + 1 == range.end;
            match (op.info().end, last) {
                (End::Yes, false) => return Err(inst(view, at, "a terminator before its block's end")),
                (End::No, true) => return Err(block(b, "a block whose last instruction is not a terminator")),
                (End::Yes, true) | (End::No, false) => {}
            }
        }
        if kind.is_landing() && (params.len() != 1 || view.ty(Value(params.start as u32)) != Ty::PTR) {
            return Err(block(b, "a landing block whose parameters are not the one exception pointer"));
        }
        if b == 0 {
            if kind.is_landing() {
                return Err(block(0, "an entry block that is a landing block"));
            }
            let tys = params.clone().map(|at| view.tys[at]);
            if !tys.eq(own.params.iter().map(|it| it.ty)) {
                return Err(block(0, "entry parameters that are not the signature's"));
            }
        }
    }
    let cfg = Cfg::of(view);
    let of = block_of(view);
    // Edges.
    for b in 0..view.blocks() {
        let at = view.terminator(Block(b as u32));
        let mut bad = None;
        edges(view, at, |edge| {
            if bad.is_some() {
                return;
            }
            let kind = view.block_kind(edge.to);
            let params = view.params(edge.to);
            if edge.to.0 == 0 {
                bad = Some("an edge to the entry block, which has no predecessors");
                return;
            }
            bad = match edge.kind {
                EdgeKind::Plain(args) => {
                    if kind.is_landing() {
                        Some("a jump to a landing block, which only unwind edges reach")
                    } else if args.len() != params.len()
                        || !args.iter().zip(params).all(|(arg, at)| view.ty(arg).0 == view.tys[at])
                    {
                        Some("arguments that are not their block's parameters")
                    } else {
                        None
                    }
                }
                EdgeKind::Normal => {
                    if kind.is_landing() || !params.is_empty() {
                        Some("a return edge to a block with parameters or a landing block")
                    } else if cfg.preds[edge.to.index()] != 1 {
                        Some("a return edge to a block with another predecessor")
                    } else {
                        None
                    }
                }
                EdgeKind::Unwind => (!kind.is_landing()).then_some("an unwind edge to a block that is not a landing block"),
                EdgeKind::Label => (kind.is_landing() || !params.is_empty())
                    .then_some("a label to a block with parameters or a landing block"),
            };
        });
        if let Some(rule) = bad {
            return Err(inst(view, at, rule));
        }
    }
    // Instructions.
    for at in 0..view.len() {
        let op = view.op(at);
        let info = op.info();
        let flags = view.flags(at);
        if let Some(rule) = info.flags.check(flags) {
            return Err(inst(view, at, rule));
        }
        let ty = Ty(view.tys[at]);
        match info.result {
            Res::None if !ty.is_none() => return Err(inst(view, at, "a result type on an operation that makes none")),
            Res::Some if ty.is_none() => return Err(inst(view, at, "no result type on an operation that makes one")),
            Res::None | Res::Some | Res::Any => {}
        }
        types(view, at, op, ty, ptr_int, &own).map_err(|rule| inst(view, at, rule))?;
        // Every value used is defined where it dominates the use.
        if !cfg.reachable(of[at]) {
            continue;
        }
        let mut bad = None;
        let mut visit = |word: Word| -> std::result::Result<(), ()> {
            if let Word::Value(value) = word {
                if let Some(rule) = dominance(view, &cfg, &of, value as usize, at) {
                    bad = Some(rule);
                    return Err(());
                }
            }
            Ok(())
        };
        match info.layout {
            Layout::None => {}
            Layout::Data(field) => {
                if field == Field::Value {
                    let _ = visit(Word::Value(view.data[at]));
                }
            }
            Layout::Extra(fields) => {
                let words = &view.extra[view.data[at] as usize..];
                let _ = walk(words, fields, ty, view.tys, &mut visit);
            }
        }
        if let Some(rule) = bad {
            return Err(inst(view, at, rule));
        }
    }
    Ok(())
}

/// Why the use of `value` at instruction `at` is not dominated by its definition, if it is
/// not.
fn dominance(view: &View<'_>, cfg: &Cfg, of: &[u32], value: usize, at: usize) -> Option<&'static str> {
    if view.ty(Value(value as u32)).is_none() {
        return Some("a use of an instruction that makes no value");
    }
    let defining = view.op(value);
    // An invoke's or asm_br's value is defined at the head of the block it returns to.
    let (def_block, def_at) = if matches!(defining, Op::Invoke | Op::InvokeIndirect | Op::AsmBr) {
        let mut operands = view.operands(value);
        operands.word();
        if defining != Op::AsmBr {
            operands.sig();
        }
        let normal = operands.block();
        (normal.0, view.block(normal).start)
    } else {
        (of[value], value + 1)
    };
    let use_block = of[at];
    if !cfg.reachable(def_block) {
        return Some("a use of a value of an unreachable block");
    }
    let ok = match def_block == use_block {
        true => def_at <= at,
        false => cfg.dominates(def_block, use_block),
    };
    (!ok).then_some("a use its value does not dominate")
}

fn same_lanes(a: Ty, b: Ty) -> bool {
    a.form() == b.form() && a.lanes() == b.lanes()
}

fn is_int_ty(ty: Ty) -> bool {
    ty.is_int() && !matches!(ty.form(), Form::Compound)
}

fn is_float_ty(ty: Ty) -> bool {
    ty.is_float() && !matches!(ty.form(), Form::Compound)
}

/// Checks the types of instruction `at`'s operands and result.
fn types(
    view: &View<'_>,
    at: usize,
    op: Op,
    ty: Ty,
    ptr_int: Ty,
    own: &crate::func::Sig<'_>,
) -> std::result::Result<(), &'static str> {
    let mut o = view.operands(at);
    let t = |v: Value| view.ty(v);
    let need = |ok: bool, rule: &'static str| if ok { Ok(()) } else { Err(rule) };
    let ptr_bits = view.head.ptr_bits;
    let flags = view.flags(at);
    match op {
        Op::Param => need(!ty.is_none(), "a parameter of no type"),
        Op::Iconst => {
            need(ty.is_scalar() && ty.is_int(), "an integer constant of a type that is not a scalar integer")?;
            let bits = ty.bits(ptr_bits);
            need(bits >= 32 || (o.imm() as u64) < (1u64 << bits), "a constant with bits past its width")
        }
        Op::IconstWide => {
            need(ty == Ty::I64 || ty == Ty::I128, "a wide constant of a type that is not i64 or i128")?;
            let words = o.constant();
            let value = words.iter().rev().fold(0u128, |all, &w| (all << 32) | w as u128);
            let shift = 128 - ty.bits(ptr_bits);
            let signed = ((value << shift) as i128) >> shift;
            need(i32::try_from(signed).is_err(), "a wide constant that fits `iconst`'s word")
        }
        Op::Fconst => {
            need(ty == Ty::F16 || ty == Ty::F32, "an `fconst` of a type that is not f16 or f32")?;
            need(ty == Ty::F32 || o.imm() <= u16::MAX as u32, "an f16 constant with bits past its width")
        }
        Op::FconstWide => need(ty == Ty::F64 || ty == Ty::F128, "an `fconst_wide` of a type that is not f64 or f128"),
        Op::Vconst => {
            need(matches!(ty.form(), Form::Vector) && !ty.is_ptr(), "a vector constant of a type that is not a fixed vector")?;
            let bits = ty.bits(ptr_bits);
            let words = o.constant();
            let spare = (32 * words.len() as u32) - bits;
            let last = words.last().copied().unwrap_or(0);
            need(spare == 0 || last >> (32 - spare) == 0, "a vector constant with bits past its width")
        }
        Op::Null => need(ty == Ty::PTR, "a null of a type that is not `ptr`"),
        Op::Uninit => need(!ty.is_none(), "an `uninit` of no type"),
        Op::Add | Op::Sub | Op::Mul | Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem | Op::And | Op::Or | Op::Xor
        | Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr | Op::Umulhi | Op::Smulhi | Op::UaddSat
        | Op::SaddSat | Op::UsubSat | Op::SsubSat | Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
            let (a, b) = (o.value(), o.value());
            need(is_int_ty(ty) && t(a) == ty && t(b) == ty, "integer operands and a result not all of one integer type")
        }
        Op::Neg | Op::Not | Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => {
            need(is_int_ty(ty) && t(o.value()) == ty, "an integer operand not of its result's type")
        }
        Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow | Op::UmulOverflow
        | Op::SmulOverflow => {
            let (a, b) = (o.value(), o.value());
            let members = view.members(ty);
            need(
                is_int_ty(t(a)) && t(a) == t(b) && members == [t(a).0, t(a).with_elem(Scalar::I1).0],
                "an overflow operation whose result is not {T, i1} of its operands' type",
            )
        }
        Op::Icmp => {
            o.word();
            let (a, b) = (o.value(), o.value());
            need(
                t(a) == t(b) && (is_int_ty(t(a)) || t(a).is_ptr()) && ty == t(a).with_elem(Scalar::I1),
                "a comparison of operands of two types, or not integers or pointers, or not giving i1",
            )
        }
        Op::Fcmp => {
            o.word();
            let (a, b) = (o.value(), o.value());
            need(
                t(a) == t(b) && is_float_ty(t(a)) && ty == t(a).with_elem(Scalar::I1),
                "a float comparison of operands of two types, or not floats, or not giving i1",
            )
        }
        Op::Select => {
            let (c, a, b) = (o.value(), o.value(), o.value());
            need(t(a) == ty && t(b) == ty, "values of a select not of its type")?;
            need(t(c) == Ty::I1 || (ty.is_vector() && t(c) == ty.with_elem(Scalar::I1)), "a select's condition that is not i1 or its lanes' i1")
        }
        Op::Zext | Op::Sext | Op::Trunc => {
            let a = t(o.value());
            let (from, to) = (a.elem().bits(ptr_bits), ty.elem().bits(ptr_bits));
            let widens = if op == Op::Trunc { to < from } else { to > from };
            need(is_int_ty(a) && is_int_ty(ty) && same_lanes(a, ty) && widens, "an integer conversion that does not change its width the way its operation says")
        }
        Op::Fpext | Op::Fptrunc => {
            let a = t(o.value());
            let (from, to) = (a.elem().bits(ptr_bits), ty.elem().bits(ptr_bits));
            let widens = if op == Op::Fptrunc { to < from } else { to > from };
            need(is_float_ty(a) && is_float_ty(ty) && same_lanes(a, ty) && widens, "a float conversion that does not change its width the way its operation says")
        }
        Op::FptoUi | Op::FptoSi => {
            let a = t(o.value());
            need(is_float_ty(a) && is_int_ty(ty) && same_lanes(a, ty), "a float to integer conversion of other types")
        }
        Op::UitoFp | Op::SitoFp => {
            let a = t(o.value());
            need(is_int_ty(a) && is_float_ty(ty) && same_lanes(a, ty), "an integer to float conversion of other types")
        }
        Op::Bitcast => {
            let a = t(o.value());
            need(
                !a.is_ptr() && !ty.is_ptr() && a.is_valid() && !a.is_none() && a.compound_row().is_none()
                    && ty.compound_row().is_none() && !matches!(a.form(), Form::Scalable)
                    && !matches!(ty.form(), Form::Scalable)
                    && a.bits(ptr_bits) == ty.bits(ptr_bits),
                "a bitcast between types of different widths, or of a pointer",
            )
        }
        Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Frem | Op::FminNum | Op::FmaxNum | Op::Fminimum
        | Op::Fmaximum | Op::Fcopysign => {
            let (a, b) = (o.value(), o.value());
            need(is_float_ty(ty) && t(a) == ty && t(b) == ty, "float operands and a result not all of one float type")
        }
        Op::Fneg | Op::Fabs | Op::Sqrt | Op::Floor | Op::Ceil | Op::Ftrunc | Op::Nearest | Op::Fround => {
            need(is_float_ty(ty) && t(o.value()) == ty, "a float operand not of its result's type")
        }
        Op::Fma | Op::Fmuladd => {
            let (a, b, c) = (o.value(), o.value(), o.value());
            need(is_float_ty(ty) && t(a) == ty && t(b) == ty && t(c) == ty, "float operands and a result not all of one float type")
        }
        Op::Freeze => need(t(o.value()) == ty, "a freeze not of its operand's type"),
        Op::Assume => need(t(o.value()) == Ty::I1, "an assume of a value that is not i1"),
        Op::PtrAdd => {
            let (p, off) = (t(o.value()), t(o.value()));
            need(p.is_ptr() && ty == p && is_int_ty(off) && (p.is_scalar() == off.is_scalar()) && same_lanes(p, off), "a ptr_add that is not a pointer plus an integer")
        }
        Op::PtrAddr | Op::Expose => {
            let p = t(o.value());
            need(p.is_ptr() && ty == p.with_elem(ptr_int.elem()), "an address of a value that is not a pointer, or not of the pointer's width")
        }
        Op::FromExposed => {
            let a = t(o.value());
            need(ty.is_ptr() && a == ty.with_elem(ptr_int.elem()), "a pointer from an integer not of the pointer's width")
        }
        Op::PtrWithAddr => {
            let (p, a) = (t(o.value()), t(o.value()));
            need(p == Ty::PTR && a == ptr_int && ty == Ty::PTR, "a ptr_with_addr not of a pointer and an address")
        }
        Op::FuncAddr | Op::DataAddr | Op::TlsAddr => {
            let rec = view.reference(o.reference());
            let kind = RefKind::from_word(rec.kind);
            let ok = if op == Op::FuncAddr {
                kind == Some(RefKind::Func)
            } else if op == Op::DataAddr {
                matches!(kind, Some(RefKind::Static | RefKind::Data))
            } else {
                kind == Some(RefKind::Tls)
            };
            need(ok && ty == Ty::PTR, "an address of a reference of another kind")
        }
        Op::SlotAddr => need(ty == Ty::PTR, "a slot address that is not a pointer"),
        Op::SlotLive | Op::SlotDead => Ok(()),
        Op::Load => {
            let p = t(o.value());
            need(p == Ty::PTR && ty.compound_row().is_none(), "a load from a value that is not a pointer, or of a compound")
        }
        Op::Store => {
            let (v, p) = (t(o.value()), t(o.value()));
            need(p == Ty::PTR && !v.is_none() && v.compound_row().is_none(), "a store to a value that is not a pointer, or of a compound")
        }
        Op::Memcpy | Op::Memmove => {
            let (d, s, n) = (t(o.value()), t(o.value()), t(o.value()));
            need(d == Ty::PTR && s == Ty::PTR && n.is_scalar() && n.is_int(), "a copy not of two pointers and an integer length")
        }
        Op::Memset => {
            let (d, byte, n) = (t(o.value()), t(o.value()), t(o.value()));
            need(d == Ty::PTR && byte == Ty::I8 && n.is_scalar() && n.is_int(), "a memset not of a pointer, an i8 and an integer length")
        }
        Op::Memcmp => {
            let (a, b, n) = (t(o.value()), t(o.value()), t(o.value()));
            need(a == Ty::PTR && b == Ty::PTR && n.is_scalar() && n.is_int() && ty == Ty::I32, "a memcmp not of two pointers and a length, giving i32")
        }
        Op::AtomicLoad => {
            need(t(o.value()) == Ty::PTR && ty.is_scalar(), "an atomic load not of a scalar from a pointer")?;
            need(!matches!(flags.ordering(), Some(Ordering::Release | Ordering::AcqRel)), "an atomic load that releases")
        }
        Op::AtomicStore => {
            let (v, p) = (t(o.value()), t(o.value()));
            need(p == Ty::PTR && v.is_scalar(), "an atomic store not of a scalar to a pointer")?;
            need(!matches!(flags.ordering(), Some(Ordering::Acquire | Ordering::AcqRel)), "an atomic store that acquires")
        }
        Op::AtomicRmw => {
            o.word();
            let (p, v) = (t(o.value()), t(o.value()));
            need(p == Ty::PTR && v.is_scalar() && (v.is_int() || v.is_ptr()) && ty == v, "an atomic read-modify-write not of an integer at a pointer")
        }
        Op::Cmpxchg => {
            let (p, e, n) = (t(o.value()), t(o.value()), t(o.value()));
            need(
                p == Ty::PTR && e == n && e.is_scalar() && (e.is_int() || e.is_ptr()) && view.members(ty) == [e.0, Ty::I1.0],
                "a cmpxchg not of an integer at a pointer, giving {T, i1}",
            )
        }
        Op::Fence => need(flags.ordering() != Some(Ordering::Relaxed), "a relaxed fence, which orders nothing"),
        Op::Splat => need(ty.is_vector() && t(o.value()) == ty.lane(), "a splat of a value that is not its vector's lane"),
        Op::Extract => {
            let v = t(o.value());
            let lane = o.imm();
            need(v.is_vector() && ty == v.lane() && lane < v.lanes(), "an extract of a lane its vector has not")
        }
        Op::Insert => {
            let (v, x) = (t(o.value()), t(o.value()));
            let lane = o.imm();
            need(v.is_vector() && ty == v && x == v.lane() && lane < v.lanes(), "an insert of a lane its vector has not")
        }
        Op::Shuffle => {
            let (a, b) = (t(o.value()), t(o.value()));
            let mask = o.mask();
            need(
                a == b && matches!(a.form(), Form::Vector) && matches!(ty.form(), Form::Vector) && ty.elem() == a.elem()
                    && mask.len() == ty.lanes() as usize && mask.iter().all(|&it| it < 2 * a.lanes()),
                "a shuffle whose lanes are not its operands' or its result's",
            )
        }
        Op::Swizzle => {
            let (a, b) = (t(o.value()), t(o.value()));
            need(matches!(a.form(), Form::Vector) && is_int_ty(b) && same_lanes(a, b) && ty == a, "a swizzle not of a vector by integer lanes")
        }
        Op::ReduceAdd | Op::ReduceMul | Op::ReduceAnd | Op::ReduceOr | Op::ReduceXor | Op::ReduceUmin
        | Op::ReduceUmax | Op::ReduceSmin | Op::ReduceSmax => {
            let v = t(o.value());
            need(v.is_vector() && v.is_int() && ty == v.lane(), "a reduction not of an integer vector to its lane")
        }
        Op::ReduceFadd | Op::ReduceFmul => {
            let (s, v) = (t(o.value()), t(o.value()));
            need(v.is_vector() && v.is_float() && s == v.lane() && ty == s, "a float reduction not of a start and a vector of its type")
        }
        Op::ReduceFmin | Op::ReduceFmax => {
            let v = t(o.value());
            need(v.is_vector() && v.is_float() && ty == v.lane(), "a float reduction not of a float vector to its lane")
        }
        Op::Bitmask => {
            let v = t(o.value());
            need(v.is_vector() && v.elem() == Scalar::I1 && ty.is_scalar() && ty.is_int() && ty.bits(ptr_bits) >= v.lanes(), "a bitmask not of an i1 vector to an integer wide enough")
        }
        Op::MaskedLoad | Op::Gather => {
            let (p, m, pass) = (t(o.value()), t(o.value()), t(o.value()));
            let addresses = if op == Op::Gather { p.is_ptr() && same_lanes(p, ty) } else { p == Ty::PTR };
            need(ty.is_vector() && addresses && m == ty.with_elem(Scalar::I1) && pass == ty, "a masked load whose address, mask or pass-through is not its vector's")
        }
        Op::MaskedStore | Op::Scatter => {
            let (v, p, m) = (t(o.value()), t(o.value()), t(o.value()));
            let addresses = if op == Op::Scatter { p.is_ptr() && same_lanes(p, v) } else { p == Ty::PTR };
            need(v.is_vector() && addresses && m == v.with_elem(Scalar::I1), "a masked store whose address or mask is not its vector's")
        }
        Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect | Op::TailCall | Op::TailCallIndirect => {
            let callee_ok = if matches!(op, Op::Call | Op::Invoke | Op::TailCall) {
                RefKind::from_word(view.reference(o.reference()).kind) == Some(RefKind::Func)
            } else {
                t(o.value()) == Ty::PTR
            };
            need(callee_ok, "a callee that is not a function or a pointer")?;
            let sig = o.sig();
            if matches!(op, Op::Invoke | Op::InvokeIndirect) {
                o.block();
                o.block();
            }
            let args = o.values();
            arguments(view, sig, args)?;
            if matches!(op, Op::TailCall | Op::TailCallIndirect) {
                need(
                    view.sig(sig).rets.iter().map(|it| it.ty).eq(own.rets.iter().map(|it| it.ty)),
                    "a tail call whose callee returns other than the caller",
                )
            } else {
                need(ty == returns(view, sig), "a call's result that is not its signature's returns")
            }
        }
        Op::TargetIntrinsic => {
            let sig = SigId(view.intrinsics[o.intrinsic().index()].sig);
            arguments(view, sig, o.values())?;
            need(ty == returns(view, sig), "an intrinsic's result that is not its signature's returns")
        }
        Op::Asm | Op::AsmBr => {
            let asm = o.asm();
            if op == Op::AsmBr {
                o.block();
                o.blocks();
            }
            let inputs = o.values();
            let operands = view.asm_operands_of(asm);
            let kinds = || operands.iter().filter_map(|it| AsmOperandKind::from_byte(it.kind));
            need(inputs.len() == kinds().filter(|it| it.takes()).count(), "inputs that are not its asm block's")?;
            let outputs = kinds().filter(|it| it.gives()).count();
            let ok = match outputs {
                0 => ty.is_none(),
                1 => !ty.is_none() && ty.compound_row().is_none(),
                n => view.members(ty).len() == n,
            };
            need(ok, "outputs that are not its asm block's")
        }
        Op::Project => {
            let v = t(o.value());
            let k = o.imm() as usize;
            need(view.members(v).get(k) == Some(&ty.0), "a projection of a member its compound has not")
        }
        Op::FrameAddress | Op::ReturnAddress | Op::StackPointer => need(ty == Ty::PTR, "a machine address that is not a pointer"),
        Op::Jump | Op::Unreachable | Op::Trap => Ok(()),
        Op::Br => need(t(o.value()) == Ty::I1, "a branch on a value that is not i1"),
        Op::Switch => {
            let scrutinee = o.value();
            let s = t(scrutinee);
            need(s.is_scalar() && s.is_int(), "a switch on a value that is not a scalar integer")?;
            o.target();
            let bits = s.bits(ptr_bits);
            let mut seen: Vec<u128> = Vec::new(); // alloc: the verifier's scratch, one switch's cases
            for (words, _) in o.cases(scrutinee) {
                let value = words.iter().rev().fold(0u128, |all, &w| (all << 32) | w as u128);
                need(bits >= 128 || value >> bits == 0, "a case value with bits past its scrutinee's width")?;
                seen.push(value);
            }
            seen.sort_unstable();
            need(seen.windows(2).all(|it| it[0] != it[1]), "two cases of one value")
        }
        Op::Return => {
            let values = o.values();
            need(
                values.len() == own.rets.len() && values.iter().zip(own.rets).all(|(v, r)| t(v).0 == r.ty),
                "returns that are not its signature's",
            )
        }
        Op::Resume => need(t(o.value()) == Ty::PTR, "a resume of a value that is not the exception pointer"),
    }
}

/// Whether `args` are what `sig` takes: its parameters, and past them for a variadic one.
fn arguments(view: &View<'_>, sig: SigId, args: Values<'_>) -> std::result::Result<(), &'static str> {
    let sig = view.sig(sig);
    let variadic = sig.bits & SigRec::VARIADIC != 0;
    let count_ok = match variadic {
        true => args.len() >= sig.params.len(),
        false => args.len() == sig.params.len(),
    };
    if !count_ok || !args.iter().zip(sig.params).all(|(arg, param)| view.ty(arg).0 == param.ty) {
        return Err("arguments that are not its signature's parameters");
    }
    Ok(())
}

/// The result type of a call of `sig` in `view`: none, its one return, or the compound of
/// its returns (which must be in `compounds`).
fn returns(view: &View<'_>, sig: SigId) -> Ty {
    let rets: &[ParamRec] = view.sig(sig).rets;
    match rets {
        [] => Ty::NONE,
        [one] => Ty(one.ty),
        many => (0..view.compounds.len())
            .map(|row| Ty::compound(row as u32).unwrap_or(Ty::NONE))
            .find(|&ty| view.members(ty).iter().copied().eq(many.iter().map(|it| it.ty)))
            .unwrap_or(Ty(u16::MAX)),
    }
}
