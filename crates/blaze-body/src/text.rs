//! The body's text form: [`print`] and [`parse`], with an exact round trip: the text [`print`]
//! writes parses to a body that prints the same text. Tests are written in it, and a body is
//! read in it when one is wrong. Places are not numbered in the text: each is written as its
//! local and projections, and [`parse`] interns them in the order they first appear.
//!
//! The grammar, one item per line; `N` is a decimal number, `H` a hexadecimal one, `T` a type
//! handle (`tN`), `P` a place, `O` an operand, `U` an unwind, `B` a block (`bbN`):
//!
//! ```text
//! body    := [types] local* const* origin* known* mentions* (header stmt+)+
//! types   := "types tN lN" NL ["tylists" (" T")+ NL] ("ty T = " node NL)*
//!                                    (the record's type and list-entry counts, the overlay's
//!                                     list entries, then each overlay type in order)
//! node    := "bool" | "char" | "str" | "never" | "infer" | "error" | int | uint | float
//!          | "tuple " L | "ref " ["mut "] T | "ptr " ("const "|"mut ") T | "array " T " " T
//!          | "slice " T | "pat " T " " L | "adt " item | "fndef " item | "opaque " item
//!          | "extrait " item | "fnptr " L " 0xH" | "closure " item " N" | "coroutine " item " N"
//!          | "future eN " T " " T | "param N" | "constparam N" | "alias " item " nH"
//!          | "exproj " item " nH " T | "constvalue " T " 0xH" | "dynamic " L | "exauto kH"
//!          | "foreign kH"
//! L       := "<N,N>"                         (a list's start and length)
//! local   := "local _N: T " kind [" mut"] [" !"]   ("!": its own place is of a Drop type)
//! kind    := "return" | "arg N" | "binding patN" | "temp eN " role | "flag _N"
//! role    := "value" | "borrow" | "awaitee" | "pin" | "result"
//! const   := "const cN = " ckind ": T"
//! ckind   := "scalar 0xH" | "bytes \"" (HH)* "\"" | "item " item | "promoted N" | "fn " item
//!          | "static kH" | "anon eN" | "param N"
//! item    := "kH<N,N>"                      (key, then the args list's start and length)
//! origin  := "origin 'oN" [" placeholder"]
//! known   := "known 'oN: 'oN"
//! mentions:= "mentions _N 'oN"
//! header  := "bbN" [" cleanup"] ":"
//! stmt    := "  " row [" @N..N"] [" ; 'oN: 'oN" (", 'oN: 'oN")*]   (span, then subsets)
//! row     := P " = " value | "live _N" | "dead _N" | "activate LN" | "fakeread " P
//!          | "setdiscr " P " variant N" | "deinit " P
//!          | "goto B" | "switch " O " [" [N " -> " B (", " N " -> " B)*] "] else " B
//!          | "call " callee ops [" -> " P] [" then " B] " unwind " U
//!          | "drop " P " then " B " unwind " U
//!          | "assert " O " == " ("true"|"false") ", " akind ops " then " B " unwind " U
//!          | "suspend resume " B " cancel " B | "return" | "unreachable" | "resume" | "terminate"
//!          | "asm N" ops [" then " B] " unwind " U
//! callee  := "fn " item | "method " item | "intrinsic " item | "value " O
//! akind   := "overflow_" binop | "overflow_neg" | "bounds" | "div_zero" | "rem_zero"
//!          | "resumed_after_return" | "resumed_after_panic"
//! U       := "continue" | "unreachable" | "terminate" | B
//! value   := O | "borrow " bkind " " P " LN 'oN" | "raw const " P | "raw mut " P
//!          | binop "(" O ", " O ")" | "checked " binop "(" O ", " O ")" | unop "(" O ")"
//!          | "cast " castkind "(" O ") as T" | ("tuple "|"array "|"closure "|"coroutine ") T ops
//!          | "adt " T "#N" ops | "repeat(" O ", cN)" | "len " P | "discr " P | "size " T
//!          | "align " T | "tls cN"
//! O       := "copy " P | "move " P | "const cN"
//! ops     := "(" [O (", " O)*] ")"
//! P       := "_N" proj*
//! proj    := (".*" | ".box" | ".N:T" | "[_N]" | "[N of N]" | "[-N of N]" | "[N..N]" | "[N..-N]"
//!          | "#N") ["!"]                    ("!": a field of a type that implements Drop)
//! ```
//!
//! `bkind`, `binop`, `unop` and `castkind` are the names in [`crate::code`]'s tables. A borrow
//! names its loan, which must be the next one, so a loan's row is the borrow's. Blocks may be
//! written in any order; each `bbN` of `0..blocks` once. [`print`] writes the blocks in the
//! order of their rows.
#![blaze::sans_io]

use blaze_own::Vec;
use blaze_typeck::{FnPtrHeader, Name, PNode};
use core::alloc::Allocator;
use core::fmt::{self, Write};

use crate::code::{FLOAT_TYS, INT_TYS, UINT_TYS, encode_ty};

use crate::body::{
    AggregateKind, AssertKind, BadRow, BinOp, Block, BlockKind, Body, BodyRef, BorrowKind, Callee,
    CastKind, Const, ConstDecl, ConstKind, ExprId, ItemKey, List, Loan, Local, LocalDecl, LocalKind,
    Mutbl, NONE, Operand, Ops, Origin, PTy, PatId, Place, Proj, Role, Span, Stmt, StmtKind, Table,
    UnOp, Unwind, Value,
};
use crate::build::{BodyBuilder, BuildError};

/// Why a body could not be printed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrintError {
    /// The sink refused a write.
    Fmt,
    /// A row that is no form of its table (a body [`crate::verify::verify`] refuses).
    Bad(BadRow),
}

impl From<fmt::Error> for PrintError {
    fn from(_: fmt::Error) -> PrintError {
        PrintError::Fmt
    }
}

impl From<BadRow> for PrintError {
    fn from(b: BadRow) -> PrintError {
        PrintError::Bad(b)
    }
}

/// What a line of text got wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseWhat {
    /// The grammar wants this here.
    Expected(&'static str),
    /// A number that does not fit or is missing.
    Number,
    /// A declaration whose id is not the next of its table.
    OutOfOrder { table: Table, want: u32, found: u32 },
    /// A block header past the number of headers.
    BlockId { id: u32, blocks: u32 },
    /// A block header written twice.
    DuplicateBlock(u32),
    /// A block of `0..blocks` with no header.
    MissingBlock(u32),
    /// The builder refused the line.
    Build(BuildError),
}

/// Where text is not a body: the line and column (from 1) and what is wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParseError {
    pub line: u32,
    pub col: u32,
    pub what: ParseWhat,
}

/// Writes `body` in the text form.
pub fn print<W: Write>(body: &BodyRef<'_>, w: &mut W) -> Result<(), PrintError> {
    let b = body;
    types(b, w)?;
    for l in 0..b.locals() {
        let d = b.local(Local(l))?;
        write!(w, "local {}: t{} ", Local(l), d.ty.0)?;
        match d.kind {
            LocalKind::Return => w.write_str("return")?,
            LocalKind::Arg(i) => write!(w, "arg {i}")?,
            LocalKind::Binding(p) => write!(w, "binding pat{}", p.0)?,
            LocalKind::Temp(e, role) => write!(w, "temp e{} {}", e.0, role.name())?,
            LocalKind::DropFlag(of) => write!(w, "flag {of}")?,
        }
        if d.mutbl == Mutbl::Mut {
            w.write_str(" mut")?;
        }
        if b.place(b.local_place(Local(l))?)?.of_drop_type {
            w.write_str(" !")?;
        }
        w.write_str("\n")?;
    }
    for c in 0..b.consts() {
        let d = b.konst(Const(c))?;
        write!(w, "const {} = ", Const(c))?;
        match d.kind {
            ConstKind::Scalar { bits } => write!(w, "scalar 0x{bits:x}")?,
            ConstKind::Bytes { .. } => {
                w.write_str("bytes \"")?;
                for byte in b.bytes(Const(c))? {
                    write!(w, "{byte:02x}")?;
                }
                w.write_str("\"")?;
            }
            ConstKind::Item { key, args } => item(w, "item", key, args)?,
            ConstKind::Promoted(i) => write!(w, "promoted {i}")?,
            ConstKind::FnItem { key, args } => item(w, "fn", key, args)?,
            ConstKind::StaticAddr(key) => write!(w, "static k{:x}", key.0)?,
            ConstKind::Anon(e) => write!(w, "anon e{}", e.0)?,
            ConstKind::Param(i) => write!(w, "param {i}")?,
        }
        writeln!(w, ": t{}", d.ty.0)?;
    }
    for o in 0..b.origins() {
        write!(w, "origin {}", Origin(o))?;
        if b.placeholder(Origin(o))? {
            w.write_str(" placeholder")?;
        }
        w.write_str("\n")?;
    }
    for (from, into) in b.known() {
        writeln!(w, "known {from}: {into}")?;
    }
    for (l, o) in b.local_origins() {
        writeln!(w, "mentions {l} {o}")?;
    }
    for s in 0..b.stmts() {
        let x = b.block_of(Stmt(s))?;
        let row = b.block(x)?;
        if row.stmts.start == s {
            write!(w, "{x}")?;
            if row.kind == BlockKind::Cleanup {
                w.write_str(" cleanup")?;
            }
            w.write_str(":\n")?;
        }
        let r = b.stmt(Stmt(s))?;
        w.write_str("  ")?;
        stmt(b, r.kind, w)?;
        if r.span != Span::default() {
            write!(w, " @{}..{}", r.span.lo, r.span.hi)?;
        }
        let mut first = true;
        for sub in b.subsets().filter(|sub| sub.at == Stmt(s)) {
            w.write_str(if first { " ; " } else { ", " })?;
            write!(w, "{}: {}", sub.from, sub.into)?;
            first = false;
        }
        w.write_str("\n")?;
    }
    Ok(())
}

fn item<W: Write>(w: &mut W, word: &str, key: ItemKey, args: List) -> fmt::Result {
    write!(w, "{word} k{:x}<{},{}>", key.0, args.start, args.len)
}

/// The overlay section; nothing when the body has no overlay.
fn types<W: Write>(b: &BodyRef<'_>, w: &mut W) -> Result<(), PrintError> {
    let (Some(ty_base), Some(list_base)) = (b.ty_base(), b.list_base()) else {
        return Ok(());
    };
    writeln!(w, "types t{ty_base} l{list_base}")?;
    if b.own_list_entries() > 0 {
        w.write_str("tylists")?;
        let all = List { start: list_base, len: b.own_list_entries() };
        let bad = BadRow { table: Table::TyLists, row: 0, code: all.len };
        for t in b.own_list(all).map_err(|_| bad)?.ok_or(bad)? {
            write!(w, " t{t}")?;
        }
        w.write_str("\n")?;
    }
    for i in 0..b.own_tys() {
        let t = PTy(ty_base + i);
        let node = b.own_ty(t)?.ok_or(BadRow { table: Table::Types, row: i, code: NONE })?;
        write!(w, "ty t{} = ", t.0)?;
        ty_node(w, node)?;
        w.write_str("\n")?;
    }
    Ok(())
}

fn list<W: Write>(w: &mut W, l: List) -> fmt::Result {
    write!(w, "<{},{}>", l.start, l.len)
}

/// The text name of `x`. Every form of the three primitive enums is in its table (one row per
/// variant, `code`'s encode reads the same tables), so the search always finds it.
fn named<T: PartialEq + Copy>(table: &[(T, &'static str)], x: T) -> &'static str {
    table.iter().find_map(|&(t, name)| (t == x).then_some(name)).unwrap_or("")
}

/// One overlay node in the text form (the module's `node`).
fn ty_node<W: Write>(w: &mut W, n: PNode) -> fmt::Result {
    match n {
        PNode::Bool => w.write_str("bool"),
        PNode::Char => w.write_str("char"),
        PNode::Str => w.write_str("str"),
        PNode::Never => w.write_str("never"),
        PNode::Infer => w.write_str("infer"),
        PNode::Error => w.write_str("error"),
        PNode::Int(t) => w.write_str(named(&INT_TYS, t)),
        PNode::Uint(t) => w.write_str(named(&UINT_TYS, t)),
        PNode::Float(t) => w.write_str(named(&FLOAT_TYS, t)),
        PNode::Tuple(l) => {
            w.write_str("tuple ")?;
            list(w, l)
        }
        PNode::Record(s, l) => {
            write!(w, "record s{:x} ", s.0)?;
            list(w, l)
        }
        PNode::Union(l) => {
            w.write_str("union ")?;
            list(w, l)
        }
        PNode::Ref(m, t) => write!(w, "ref {}t{}", if m == Mutbl::Mut { "mut " } else { "" }, t.0),
        PNode::Ptr(m, t) => write!(w, "ptr {} t{}", if m == Mutbl::Mut { "mut" } else { "const" }, t.0),
        PNode::Array(t, n) => write!(w, "array t{} t{}", t.0, n.0),
        PNode::Slice(t) => write!(w, "slice t{}", t.0),
        PNode::Pat(t, l) => {
            write!(w, "pat t{} ", t.0)?;
            list(w, l)
        }
        PNode::Adt(k, l) => item(w, "adt", k, l),
        PNode::FnDef(k, l) => item(w, "fndef", k, l),
        PNode::Opaque(k, l) => item(w, "opaque", k, l),
        PNode::ExTrait(k, l) => item(w, "extrait", k, l),
        PNode::FnPtr(l, h) => {
            w.write_str("fnptr ")?;
            list(w, l)?;
            write!(w, " 0x{:x}", h.bits())
        }
        PNode::Closure { owner, ordinal, args } => {
            item(w, "closure", owner, args)?;
            write!(w, " {ordinal}")
        }
        PNode::Coroutine { owner, ordinal, args } => {
            item(w, "coroutine", owner, args)?;
            write!(w, " {ordinal}")
        }
        PNode::Future(e, a, b) => write!(w, "future e{} t{} t{}", e.0, a.0, b.0),
        PNode::Param(i) => write!(w, "param {i}"),
        PNode::ConstParam(i) => write!(w, "constparam {i}"),
        PNode::Alias { trait_, name, args } => {
            item(w, "alias", trait_, args)?;
            write!(w, " n{:x}", name.0)
        }
        PNode::ExProj { trait_, name, args, term } => {
            item(w, "exproj", trait_, args)?;
            write!(w, " n{:x} t{}", name.0, term.0)
        }
        PNode::ConstValue { ty, bits } => write!(w, "constvalue t{} 0x{bits:x}", ty.0),
        PNode::Dynamic(l) => {
            w.write_str("dynamic ")?;
            list(w, l)
        }
        PNode::ExAuto(k) => write!(w, "exauto k{:x}", k.0),
        PNode::Foreign(k) => write!(w, "foreign k{:x}", k.0),
    }
}

fn write_proj<W: Write>(w: &mut W, proj: Proj, of_drop_type: bool) -> fmt::Result {
    let minus = |from_end: bool| if from_end { "-" } else { "" };
    match proj {
        Proj::Field { index, ty } => write!(w, ".{index}:t{}", ty.0)?,
        Proj::Deref => w.write_str(".*")?,
        Proj::DerefBox => w.write_str(".box")?,
        Proj::Index(l) => write!(w, "[{l}]")?,
        Proj::ConstIndex { offset, min_length, from_end } => {
            write!(w, "[{}{offset} of {min_length}]", minus(from_end))?;
        }
        Proj::Subslice { from, to, from_end } => write!(w, "[{from}..{}{to}]", minus(from_end))?,
        Proj::Downcast(v) => write!(w, "#{v}")?,
    }
    if of_drop_type {
        w.write_str("!")?;
    }
    Ok(())
}

fn place<W: Write>(b: &BodyRef<'_>, p: Place, w: &mut W) -> Result<(), PrintError> {
    const BUF: usize = 64;
    let mut projs: [Option<(Proj, bool)>; BUF] = [None; BUF];
    let mut depth: usize = 0;
    let mut cur = p;
    let root_local = loop {
        let row = b.place(cur)?;
        match (row.parent, row.proj) {
            (None, None) => break row.local,
            // A parent is an earlier row (verify's rule), which also ends this walk.
            (Some(up), Some(proj)) if up.0 < cur.0 => {
                if let Some(slot) = projs.get_mut(depth) {
                    *slot = Some((proj, row.of_drop_type));
                }
                depth += 1;
                cur = up;
            }
            _ => return Err(PrintError::Bad(BadRow { table: Table::Places, row: cur.0, code: NONE })),
        }
    };
    write!(w, "{root_local}")?;
    if depth <= BUF {
        if let Some(slice) = projs.get(..depth) {
            for entry in slice.iter().rev() {
                if let Some((proj, of_drop_type)) = *entry {
                    write_proj(w, proj, of_drop_type)?;
                }
            }
        }
        return Ok(());
    }
    // Deep place (> BUF projections): emit in chunks from root to leaf without heap allocation.
    let mut emitted: usize = 0;
    while emitted < depth {
        let chunk = (depth - emitted).min(BUF);
        let skip = depth - emitted - chunk;
        let mut cur = p;
        for _ in 0..skip {
            let row = b.place(cur)?;
            let Some(up) = row.parent else {
                return Err(PrintError::Bad(BadRow { table: Table::Places, row: cur.0, code: NONE }));
            };
            cur = up;
        }
        let mut filled: usize = 0;
        for _ in 0..chunk {
            let row = b.place(cur)?;
            match (row.parent, row.proj) {
                (Some(up), Some(proj)) => {
                    if let Some(slot) = projs.get_mut(filled) {
                        *slot = Some((proj, row.of_drop_type));
                    }
                    filled += 1;
                    cur = up;
                }
                _ => return Err(PrintError::Bad(BadRow { table: Table::Places, row: cur.0, code: NONE })),
            }
        }
        if let Some(slice) = projs.get(..filled) {
            for entry in slice.iter().rev() {
                if let Some((proj, of_drop_type)) = *entry {
                    write_proj(w, proj, of_drop_type)?;
                }
            }
        }
        emitted += chunk;
    }
    Ok(())
}

fn operand<W: Write>(b: &BodyRef<'_>, o: Operand, w: &mut W) -> Result<(), PrintError> {
    match o {
        Operand::Copy(p) => {
            w.write_str("copy ")?;
            place(b, p, w)
        }
        Operand::Move(p) => {
            w.write_str("move ")?;
            place(b, p, w)
        }
        Operand::Const(c) => Ok(write!(w, "const {c}")?),
    }
}

fn ops<W: Write>(b: &BodyRef<'_>, o: Ops, w: &mut W) -> Result<(), PrintError> {
    w.write_str("(")?;
    for i in o.start..o.end {
        if i != o.start {
            w.write_str(", ")?;
        }
        operand(b, b.operand(i)?, w)?;
    }
    Ok(w.write_str(")")?)
}

fn unwind<W: Write>(u: Unwind, w: &mut W) -> fmt::Result {
    match u {
        Unwind::Continue => w.write_str(" unwind continue"),
        Unwind::Cleanup(x) => write!(w, " unwind {x}"),
        Unwind::Unreachable => w.write_str(" unwind unreachable"),
        Unwind::Terminate => w.write_str(" unwind terminate"),
    }
}

fn pair<W: Write>(b: &BodyRef<'_>, name: &str, lhs: Operand, rhs: Operand, w: &mut W) -> Result<(), PrintError> {
    write!(w, "{name}(")?;
    operand(b, lhs, w)?;
    w.write_str(", ")?;
    operand(b, rhs, w)?;
    Ok(w.write_str(")")?)
}

fn value<W: Write>(b: &BodyRef<'_>, v: Value, w: &mut W) -> Result<(), PrintError> {
    match v {
        Value::Use(o) => operand(b, o, w)?,
        Value::Borrow { kind, place: p, loan } => {
            write!(w, "borrow {} ", kind.name())?;
            place(b, p, w)?;
            write!(w, " {loan} {}", b.loan(loan)?.origin)?;
        }
        Value::RawAddr { mutbl, place: p } => {
            w.write_str(if mutbl == Mutbl::Mut { "raw mut " } else { "raw const " })?;
            place(b, p, w)?;
        }
        Value::Binary { op, lhs, rhs } => pair(b, op.name(), lhs, rhs, w)?,
        Value::CheckedBinary { op, lhs, rhs } => {
            w.write_str("checked ")?;
            pair(b, op.name(), lhs, rhs, w)?;
        }
        Value::Unary { op, operand: o } => {
            write!(w, "{}(", op.name())?;
            operand(b, o, w)?;
            w.write_str(")")?;
        }
        Value::Cast { kind, operand: o, ty } => {
            write!(w, "cast {}(", kind.name())?;
            operand(b, o, w)?;
            write!(w, ") as t{}", ty.0)?;
        }
        Value::Aggregate { kind, ty, operands } => {
            match kind {
                AggregateKind::Tuple => write!(w, "tuple t{}", ty.0)?,
                AggregateKind::Array => write!(w, "array t{}", ty.0)?,
                AggregateKind::Adt(v) => write!(w, "adt t{}#{v}", ty.0)?,
                AggregateKind::Closure => write!(w, "closure t{}", ty.0)?,
                AggregateKind::Coroutine => write!(w, "coroutine t{}", ty.0)?,
            }
            ops(b, operands, w)?;
        }
        Value::Repeat { operand: o, count } => {
            w.write_str("repeat(")?;
            operand(b, o, w)?;
            write!(w, ", {count})")?;
        }
        Value::Len(p) => {
            w.write_str("len ")?;
            place(b, p, w)?;
        }
        Value::Discriminant(p) => {
            w.write_str("discr ")?;
            place(b, p, w)?;
        }
        Value::SizeOf(t) => write!(w, "size t{}", t.0)?,
        Value::AlignOf(t) => write!(w, "align t{}", t.0)?,
        Value::ThreadLocalAddr(c) => write!(w, "tls {c}")?,
    }
    Ok(())
}

fn assert_name<W: Write>(k: AssertKind, w: &mut W) -> fmt::Result {
    match k {
        AssertKind::Overflow(op) => write!(w, "overflow_{}", op.name()),
        AssertKind::OverflowNeg => w.write_str("overflow_neg"),
        AssertKind::Bounds => w.write_str("bounds"),
        AssertKind::DivisionByZero => w.write_str("div_zero"),
        AssertKind::RemainderByZero => w.write_str("rem_zero"),
        AssertKind::ResumedAfterReturn => w.write_str("resumed_after_return"),
        AssertKind::ResumedAfterPanic => w.write_str("resumed_after_panic"),
    }
}

fn stmt<W: Write>(b: &BodyRef<'_>, kind: StmtKind, w: &mut W) -> Result<(), PrintError> {
    match kind {
        StmtKind::Assign { dest, value: v } => {
            place(b, dest, w)?;
            w.write_str(" = ")?;
            value(b, v, w)?;
        }
        StmtKind::StorageLive(l) => write!(w, "live {l}")?,
        StmtKind::StorageDead(l) => write!(w, "dead {l}")?,
        StmtKind::Activate(l) => write!(w, "activate {l}")?,
        StmtKind::FakeRead(p) => {
            w.write_str("fakeread ")?;
            place(b, p, w)?;
        }
        StmtKind::SetDiscriminant { place: p, variant } => {
            w.write_str("setdiscr ")?;
            place(b, p, w)?;
            write!(w, " variant {variant}")?;
        }
        StmtKind::Deinit(p) => {
            w.write_str("deinit ")?;
            place(b, p, w)?;
        }
        StmtKind::Goto(t) => write!(w, "goto {t}")?,
        StmtKind::Switch { on, cases, otherwise } => {
            w.write_str("switch ")?;
            operand(b, on, w)?;
            w.write_str(" [")?;
            for i in cases.start..cases.end {
                if i != cases.start {
                    w.write_str(", ")?;
                }
                let (v, t) = b.case(i)?;
                write!(w, "{v} -> {t}")?;
            }
            write!(w, "] else {otherwise}")?;
        }
        StmtKind::Call { callee, args, dest, target, unwind: u } => {
            w.write_str("call ")?;
            match b.callee(callee)? {
                Callee::FnItem { key, args } => item(w, "fn", key, args)?,
                Callee::Method { key, args } => item(w, "method", key, args)?,
                Callee::Intrinsic { key, args } => item(w, "intrinsic", key, args)?,
                Callee::Value(o) => {
                    w.write_str("value ")?;
                    operand(b, o, w)?;
                }
            }
            ops(b, args, w)?;
            if let Some(p) = dest {
                w.write_str(" -> ")?;
                place(b, p, w)?;
            }
            if let Some(t) = target {
                write!(w, " then {t}")?;
            }
            unwind(u, w)?;
        }
        StmtKind::Drop { place: p, target, unwind: u } => {
            w.write_str("drop ")?;
            place(b, p, w)?;
            write!(w, " then {target}")?;
            unwind(u, w)?;
        }
        StmtKind::Assert { cond, expected, kind, msg, target, unwind: u } => {
            w.write_str("assert ")?;
            operand(b, cond, w)?;
            write!(w, " == {expected}, ")?;
            assert_name(kind, w)?;
            ops(b, msg, w)?;
            write!(w, " then {target}")?;
            unwind(u, w)?;
        }
        StmtKind::Suspend { resume, cancel } => write!(w, "suspend resume {resume} cancel {cancel}")?,
        StmtKind::Return => w.write_str("return")?,
        StmtKind::Unreachable => w.write_str("unreachable")?,
        StmtKind::Resume => w.write_str("resume")?,
        StmtKind::Terminate => w.write_str("terminate")?,
        StmtKind::InlineAsm { asm, operands, target, unwind: u } => {
            write!(w, "asm {asm}")?;
            ops(b, operands, w)?;
            if let Some(t) = target {
                write!(w, " then {t}")?;
            }
            unwind(u, w)?;
        }
    }
    Ok(())
}

/// A cursor over one line.
#[derive(Clone, Copy)]
struct Cur<'s> {
    line: &'s str,
    at: usize,
    no: u32,
}

impl<'s> Cur<'s> {
    fn new(line: &'s str, index: usize) -> Cur<'s> {
        Cur { line, at: 0, no: index as u32 + 1 }
    }

    fn rest(&self) -> &'s str {
        self.line.get(self.at..).unwrap_or("")
    }

    fn err(&self, what: ParseWhat) -> ParseError {
        ParseError { line: self.no, col: self.at as u32 + 1, what }
    }

    fn ws(&mut self) {
        while self.rest().starts_with(' ') {
            self.at += 1;
        }
    }

    /// `t` right here, with no space before it.
    fn tight(&mut self, t: &str) -> bool {
        let found = self.rest().starts_with(t);
        if found {
            self.at += t.len();
        }
        found
    }

    /// `t` after any spaces.
    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        self.tight(t)
    }

    fn need(&mut self, t: &'static str) -> Result<(), ParseError> {
        match self.eat(t) {
            true => Ok(()),
            false => Err(self.err(ParseWhat::Expected(t))),
        }
    }

    fn word(&mut self) -> &'s str {
        self.ws();
        let rest = self.rest();
        let n = rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'_').count();
        self.at += n;
        rest.get(..n).unwrap_or("")
    }

    /// The word `k`, consumed only when it is the next whole word.
    fn keyword(&mut self, k: &str) -> bool {
        let mut c = *self;
        let found = c.word() == k;
        if found {
            *self = c;
        }
        found
    }

    fn digits(&mut self, radix: u32) -> Result<u128, ParseError> {
        let rest = self.rest();
        let n = rest.bytes().take_while(|c| (*c as char).is_digit(radix)).count();
        let v = match n {
            0 => None,
            _ => rest.get(..n).and_then(|s| u128::from_str_radix(s, radix).ok()),
        };
        let v = v.ok_or_else(|| self.err(ParseWhat::Number))?;
        self.at += n;
        Ok(v)
    }

    /// A decimal `u32` right here.
    fn u32(&mut self) -> Result<u32, ParseError> {
        let v = self.digits(10)?;
        u32::try_from(v).map_err(|_| self.err(ParseWhat::Number))
    }

    /// A decimal `u32` after any spaces.
    fn num(&mut self) -> Result<u32, ParseError> {
        self.ws();
        self.u32()
    }

    /// `p` then a decimal `u32` (`t5`, `bb2`, `'o1`).
    fn prefixed(&mut self, p: &'static str) -> Result<u32, ParseError> {
        self.need(p)?;
        self.u32()
    }

    fn done(&mut self) -> Result<(), ParseError> {
        self.ws();
        match self.rest().is_empty() {
            true => Ok(()),
            false => Err(self.err(ParseWhat::Expected("the end of the line"))),
        }
    }
}

fn is_header(line: &str) -> bool {
    line.starts_with("bb")
}

fn header(c: &mut Cur<'_>) -> Result<(u32, BlockKind), ParseError> {
    let id = c.prefixed("bb")?;
    let kind = match c.keyword("cleanup") {
        true => BlockKind::Cleanup,
        false => BlockKind::Normal,
    };
    c.need(":")?;
    c.done()?;
    Ok((id, kind))
}

/// A row as read, before the builder takes it: a borrow carries its origin.
enum Row {
    Kind(StmtKind),
    Borrow { dest: Place, kind: BorrowKind, place: Place, loan: Loan, origin: Origin },
}

struct Parser<A: Allocator + Copy> {
    b: BodyBuilder<A>,
    ops: Vec<Operand, A>,
    cases: Vec<(u128, Block), A>,
    bytes: Vec<u8, A>,
}

/// Reads a body in the text form, its tables in `alloc`.
pub fn parse<A: Allocator + Copy>(text: &str, alloc: A) -> Result<Body<A>, ParseError> {
    let mut p = Parser {
        b: BodyBuilder::new_in(alloc),
        ops: Vec::new_in(alloc),
        cases: Vec::new_in(alloc),
        bytes: Vec::new_in(alloc),
    };
    // Blocks first, by id, so a row may name a block written after it.
    let n = text.lines().filter(|l| is_header(l)).count();
    let mut kinds: Vec<u8, A> = Vec::new_in(alloc);
    kinds.resize(n, 0);
    for (i, line) in text.lines().enumerate() {
        if !is_header(line) {
            continue;
        }
        let mut c = Cur::new(line, i);
        let (id, kind) = header(&mut c)?;
        let blocks = n as u32;
        let slot = kinds.get_mut(id as usize).ok_or_else(|| c.err(ParseWhat::BlockId { id, blocks }))?;
        if *slot != 0 {
            return Err(c.err(ParseWhat::DuplicateBlock(id)));
        }
        *slot = 1 + kind.code() as u8;
    }
    for (id, k) in kinds.iter().enumerate() {
        match BlockKind::from_code(u32::from(*k).wrapping_sub(1)) {
            Some(kind) => {
                p.b.new_block(kind);
            }
            None => return Err(ParseError { line: 0, col: 0, what: ParseWhat::MissingBlock(id as u32) }),
        }
    }
    let mut last = 0;
    for (i, line) in text.lines().enumerate() {
        last = i + 1;
        let mut c = Cur::new(line, i);
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with("  ") {
            p.stmt(&mut c)?;
        } else if is_header(line) {
            let (id, _) = header(&mut c)?;
            p.b.start(Block(id));
        } else {
            p.decl(&mut c)?;
        }
        if let Some(e) = p.b.error() {
            return Err(ParseError { line: c.no, col: 1, what: ParseWhat::Build(e) });
        }
    }
    p.b.finish().map_err(|e| ParseError { line: last as u32 + 1, col: 1, what: ParseWhat::Build(e) })
}

impl<A: Allocator + Copy> Parser<A> {
    fn next_id(c: &Cur<'_>, table: Table, want: u32, found: u32) -> Result<(), ParseError> {
        match want == found {
            true => Ok(()),
            false => Err(c.err(ParseWhat::OutOfOrder { table, want, found })),
        }
    }

    fn origin(c: &mut Cur<'_>) -> Result<Origin, ParseError> {
        c.prefixed("'o").map(Origin)
    }

    fn local(c: &mut Cur<'_>) -> Result<Local, ParseError> {
        c.prefixed("_").map(Local)
    }

    fn block(c: &mut Cur<'_>) -> Result<Block, ParseError> {
        c.prefixed("bb").map(Block)
    }

    fn ty(c: &mut Cur<'_>) -> Result<PTy, ParseError> {
        c.prefixed("t").map(PTy)
    }

    fn item(c: &mut Cur<'_>) -> Result<(ItemKey, List), ParseError> {
        c.need("k")?;
        let key = ItemKey(c.digits(16)?);
        c.need("<")?;
        let start = c.num()?;
        c.need(",")?;
        let len = c.num()?;
        c.need(">")?;
        Ok((key, List { start, len }))
    }

    fn list(c: &mut Cur<'_>) -> Result<List, ParseError> {
        c.need("<")?;
        let start = c.num()?;
        c.need(",")?;
        let len = c.num()?;
        c.need(">")?;
        Ok(List { start, len })
    }

    fn hex(c: &mut Cur<'_>, prefix: &'static str) -> Result<u128, ParseError> {
        c.need(prefix)?;
        c.digits(16)
    }

    /// One overlay node (the module's `node`).
    fn ty_node(c: &mut Cur<'_>) -> Result<PNode, ParseError> {
        fn form<T: Copy>(table: &[(T, &str)], w: &str) -> Option<T> {
            table.iter().find(|(_, n)| *n == w).map(|(t, _)| *t)
        }
        let w = c.word();
        if let Some(t) = form(&INT_TYS, w) {
            return Ok(PNode::Int(t));
        }
        if let Some(t) = form(&UINT_TYS, w) {
            return Ok(PNode::Uint(t));
        }
        if let Some(t) = form(&FLOAT_TYS, w) {
            return Ok(PNode::Float(t));
        }
        let n = match w {
            "bool" => PNode::Bool,
            "char" => PNode::Char,
            "str" => PNode::Str,
            "never" => PNode::Never,
            "infer" => PNode::Infer,
            "error" => PNode::Error,
            "tuple" => PNode::Tuple(Self::list(c)?),
            "record" => {
                let s = blaze_typeck::RecordSchema(u64::try_from(Self::hex(c, "s")?).map_err(|_| c.err(ParseWhat::Number))?);
                PNode::Record(s, Self::list(c)?)
            }
            "union" => PNode::Union(Self::list(c)?),
            "ref" => {
                let m = match c.keyword("mut") {
                    true => Mutbl::Mut,
                    false => Mutbl::Not,
                };
                PNode::Ref(m, Self::ty(c)?)
            }
            "ptr" => {
                let m = match c.word() {
                    "mut" => Mutbl::Mut,
                    "const" => Mutbl::Not,
                    _ => return Err(c.err(ParseWhat::Expected("const or mut"))),
                };
                PNode::Ptr(m, Self::ty(c)?)
            }
            "array" => PNode::Array(Self::ty(c)?, Self::ty(c)?),
            "slice" => PNode::Slice(Self::ty(c)?),
            "pat" => PNode::Pat(Self::ty(c)?, Self::list(c)?),
            "adt" | "fndef" | "opaque" | "extrait" => {
                let (k, l) = Self::item(c)?;
                match w {
                    "adt" => PNode::Adt(k, l),
                    "fndef" => PNode::FnDef(k, l),
                    "opaque" => PNode::Opaque(k, l),
                    _ => PNode::ExTrait(k, l),
                }
            }
            "fnptr" => {
                let l = Self::list(c)?;
                let bits = u8::try_from(Self::hex(c, "0x")?).map_err(|_| c.err(ParseWhat::Number))?;
                let h = FnPtrHeader::from_bits(bits).ok_or_else(|| c.err(ParseWhat::Expected("a fn pointer header")))?;
                PNode::FnPtr(l, h)
            }
            "closure" | "coroutine" => {
                let (owner, args) = Self::item(c)?;
                let ordinal = c.num()?;
                match w {
                    "closure" => PNode::Closure { owner, ordinal, args },
                    _ => PNode::Coroutine { owner, ordinal, args },
                }
            }
            "future" => PNode::Future(ExprId(c.prefixed("e")?), Self::ty(c)?, Self::ty(c)?),
            "param" => PNode::Param(c.num()?),
            "constparam" => PNode::ConstParam(c.num()?),
            "alias" | "exproj" => {
                let (trait_, args) = Self::item(c)?;
                let name = Name(u64::try_from(Self::hex(c, "n")?).map_err(|_| c.err(ParseWhat::Number))?);
                match w {
                    "alias" => PNode::Alias { trait_, name, args },
                    _ => PNode::ExProj { trait_, name, args, term: Self::ty(c)? },
                }
            }
            "constvalue" => PNode::ConstValue { ty: Self::ty(c)?, bits: Self::hex(c, "0x")? },
            "dynamic" => PNode::Dynamic(Self::list(c)?),
            "exauto" => PNode::ExAuto(ItemKey(Self::hex(c, "k")?)),
            "foreign" => PNode::Foreign(ItemKey(Self::hex(c, "k")?)),
            _ => return Err(c.err(ParseWhat::Expected("a type"))),
        };
        Ok(n)
    }

    fn decl(&mut self, c: &mut Cur<'_>) -> Result<(), ParseError> {
        let view = self.b.body().view();
        let (locals, consts, origins) = (view.locals(), view.consts(), view.origins());
        match c.word() {
            "local" => {
                let l = Self::local(c)?;
                Self::next_id(c, Table::Locals, locals, l.0)?;
                c.need(":")?;
                let ty = Self::ty(c)?;
                let kind = match c.word() {
                    "return" => LocalKind::Return,
                    "arg" => LocalKind::Arg(c.num()?),
                    "binding" => LocalKind::Binding(PatId(c.prefixed("pat")?)),
                    "temp" => {
                        let e = ExprId(c.prefixed("e")?);
                        let role = Role::from_name(c.word()).ok_or_else(|| c.err(ParseWhat::Expected("a role")))?;
                        LocalKind::Temp(e, role)
                    }
                    "flag" => LocalKind::DropFlag(Self::local(c)?),
                    _ => return Err(c.err(ParseWhat::Expected("a local's kind"))),
                };
                let mutbl = match c.keyword("mut") {
                    true => Mutbl::Mut,
                    false => Mutbl::Not,
                };
                let of_drop_type = c.eat("!");
                c.done()?;
                let l = self.b.new_local(LocalDecl { ty, kind, mutbl });
                if of_drop_type {
                    self.b.mark_local_drop_type(l);
                }
            }
            "const" => {
                let k = c.prefixed("c")?;
                Self::next_id(c, Table::Consts, consts, k)?;
                c.need("=")?;
                let kind = match c.word() {
                    "scalar" => {
                        c.need("0x")?;
                        Some(ConstKind::Scalar { bits: c.digits(16)? })
                    }
                    "bytes" => {
                        c.need("\"")?;
                        self.bytes.clear();
                        while !c.tight("\"") {
                            let start = c.at;
                            let pair = c.rest().get(..2).and_then(|h| u8::from_str_radix(h, 16).ok());
                            let byte = pair.ok_or_else(|| Cur { at: start, ..*c }.err(ParseWhat::Expected("a hex byte")))?;
                            self.bytes.push(byte);
                            c.at += 2;
                        }
                        None
                    }
                    "item" => {
                        let (key, args) = Self::item(c)?;
                        Some(ConstKind::Item { key, args })
                    }
                    "promoted" => Some(ConstKind::Promoted(c.num()?)),
                    "fn" => {
                        let (key, args) = Self::item(c)?;
                        Some(ConstKind::FnItem { key, args })
                    }
                    "static" => {
                        c.need("k")?;
                        Some(ConstKind::StaticAddr(ItemKey(c.digits(16)?)))
                    }
                    "anon" => Some(ConstKind::Anon(ExprId(c.prefixed("e")?))),
                    "param" => Some(ConstKind::Param(c.num()?)),
                    _ => return Err(c.err(ParseWhat::Expected("a constant's kind"))),
                };
                c.need(":")?;
                let ty = Self::ty(c)?;
                c.done()?;
                // Appended as written: a text may repeat a constant, and each keeps its number.
                match kind {
                    Some(kind) => self.b.push_const(ConstDecl { kind, ty }),
                    None => self.b.push_bytes(&self.bytes, ty),
                };
            }
            "types" => {
                let ty_base = c.prefixed("t")?;
                c.ws();
                let list_base = c.prefixed("l")?;
                c.done()?;
                self.b.set_type_bases(ty_base, list_base);
            }
            "tylists" => {
                if view.ty_base().is_none() {
                    return Err(c.err(ParseWhat::Expected("a types line first")));
                }
                loop {
                    c.ws();
                    if c.rest().is_empty() {
                        break;
                    }
                    let t = Self::ty(c)?;
                    self.b.push_ty_entry(t);
                }
            }
            "ty" => {
                let Some(ty_base) = view.ty_base() else {
                    return Err(c.err(ParseWhat::Expected("a types line first")));
                };
                let t = Self::ty(c)?;
                Self::next_id(c, Table::Types, ty_base.saturating_add(view.own_tys()), t.0)?;
                c.need("=")?;
                let node = Self::ty_node(c)?;
                c.done()?;
                self.b.push_ty_row(encode_ty(node));
            }
            "origin" => {
                let o = Self::origin(c)?;
                Self::next_id(c, Table::Origins, origins, o.0)?;
                let placeholder = c.keyword("placeholder");
                c.done()?;
                self.b.origin(placeholder);
            }
            "known" => {
                let from = Self::origin(c)?;
                c.need(":")?;
                let into = Self::origin(c)?;
                c.done()?;
                self.b.known(from, into);
            }
            "mentions" => {
                let l = Self::local(c)?;
                let o = Self::origin(c)?;
                c.done()?;
                self.b.mentions(l, o);
            }
            _ => return Err(c.err(ParseWhat::Expected("a declaration or a block"))),
        }
        Ok(())
    }

    fn place(&mut self, c: &mut Cur<'_>) -> Result<Place, ParseError> {
        let l = Self::local(c)?;
        let mut p = self.b.local_place(l);
        loop {
            let proj = if c.tight(".*") {
                Proj::Deref
            } else if c.tight(".box") {
                Proj::DerefBox
            } else if c.tight(".") {
                let index = c.u32()?;
                if !c.tight(":") {
                    return Err(c.err(ParseWhat::Expected(":")));
                }
                Proj::Field { index, ty: Self::ty(c)? }
            } else if c.tight("[") {
                let proj = if c.rest().starts_with('_') {
                    Proj::Index(Self::local(c)?)
                } else {
                    let back = c.tight("-");
                    let a = c.u32()?;
                    if c.tight("..") {
                        if back {
                            return Err(c.err(ParseWhat::Expected("a subslice's start from the front")));
                        }
                        let from_end = c.tight("-");
                        Proj::Subslice { from: a, to: c.u32()?, from_end }
                    } else {
                        c.need("of")?;
                        Proj::ConstIndex { offset: a, min_length: c.num()?, from_end: back }
                    }
                };
                if !c.tight("]") {
                    return Err(c.err(ParseWhat::Expected("]")));
                }
                proj
            } else if c.tight("#") {
                Proj::Downcast(c.u32()?)
            } else {
                return Ok(p);
            };
            let marked = c.tight("!");
            p = self.b.project_marked(p, proj, marked);
        }
    }

    fn operand(&mut self, c: &mut Cur<'_>) -> Result<Operand, ParseError> {
        match c.word() {
            "copy" => Ok(Operand::Copy(self.place(c)?)),
            "move" => Ok(Operand::Move(self.place(c)?)),
            "const" => Ok(Operand::Const(Const(c.prefixed("c")?))),
            _ => Err(c.err(ParseWhat::Expected("an operand"))),
        }
    }

    fn ops(&mut self, c: &mut Cur<'_>) -> Result<Ops, ParseError> {
        c.need("(")?;
        self.ops.clear();
        if !c.eat(")") {
            loop {
                let o = self.operand(c)?;
                self.ops.push(o);
                if c.eat(")") {
                    break;
                }
                c.need(",")?;
            }
        }
        Ok(self.b.operands(&self.ops))
    }

    fn two(&mut self, c: &mut Cur<'_>) -> Result<(Operand, Operand), ParseError> {
        c.need("(")?;
        let lhs = self.operand(c)?;
        c.need(",")?;
        let rhs = self.operand(c)?;
        c.need(")")?;
        Ok((lhs, rhs))
    }

    fn unwind(c: &mut Cur<'_>) -> Result<Unwind, ParseError> {
        if !c.keyword("unwind") {
            return Err(c.err(ParseWhat::Expected("unwind")));
        }
        if c.keyword("continue") {
            Ok(Unwind::Continue)
        } else if c.keyword("unreachable") {
            Ok(Unwind::Unreachable)
        } else if c.keyword("terminate") {
            Ok(Unwind::Terminate)
        } else {
            Self::block(c).map(Unwind::Cleanup)
        }
    }

    fn then(c: &mut Cur<'_>) -> Result<Option<Block>, ParseError> {
        match c.keyword("then") {
            true => Self::block(c).map(Some),
            false => Ok(None),
        }
    }

    fn need_then(c: &mut Cur<'_>) -> Result<Block, ParseError> {
        Self::then(c)?.ok_or_else(|| c.err(ParseWhat::Expected("then")))
    }

    fn value(&mut self, c: &mut Cur<'_>, dest: Place) -> Result<Row, ParseError> {
        let start = *c;
        let w = c.word();
        let v = match w {
            "copy" | "move" | "const" => {
                *c = start;
                Value::Use(self.operand(c)?)
            }
            "borrow" => {
                let kind = BorrowKind::from_name(c.word()).ok_or_else(|| c.err(ParseWhat::Expected("a borrow kind")))?;
                let place = self.place(c)?;
                let loan = Loan(c.prefixed("L")?);
                let origin = Self::origin(c)?;
                return Ok(Row::Borrow { dest, kind, place, loan, origin });
            }
            "raw" => {
                let mutbl = match c.word() {
                    "const" => Mutbl::Not,
                    "mut" => Mutbl::Mut,
                    _ => return Err(c.err(ParseWhat::Expected("const or mut"))),
                };
                Value::RawAddr { mutbl, place: self.place(c)? }
            }
            "checked" => {
                let op = BinOp::from_name(c.word()).ok_or_else(|| c.err(ParseWhat::Expected("an operator")))?;
                let (lhs, rhs) = self.two(c)?;
                Value::CheckedBinary { op, lhs, rhs }
            }
            "cast" => {
                let kind = CastKind::from_name(c.word()).ok_or_else(|| c.err(ParseWhat::Expected("a cast kind")))?;
                c.need("(")?;
                let operand = self.operand(c)?;
                c.need(")")?;
                if !c.keyword("as") {
                    return Err(c.err(ParseWhat::Expected("as")));
                }
                Value::Cast { kind, operand, ty: Self::ty(c)? }
            }
            "tuple" | "array" | "closure" | "coroutine" | "adt" => {
                let ty = Self::ty(c)?;
                let kind = match w {
                    "tuple" => AggregateKind::Tuple,
                    "array" => AggregateKind::Array,
                    "closure" => AggregateKind::Closure,
                    "coroutine" => AggregateKind::Coroutine,
                    _ => AggregateKind::Adt(c.prefixed("#")?),
                };
                Value::Aggregate { kind, ty, operands: self.ops(c)? }
            }
            "repeat" => {
                c.need("(")?;
                let operand = self.operand(c)?;
                c.need(",")?;
                let count = Const(c.prefixed("c")?);
                c.need(")")?;
                Value::Repeat { operand, count }
            }
            "len" => Value::Len(self.place(c)?),
            "discr" => Value::Discriminant(self.place(c)?),
            "size" => Value::SizeOf(Self::ty(c)?),
            "align" => Value::AlignOf(Self::ty(c)?),
            "tls" => Value::ThreadLocalAddr(Const(c.prefixed("c")?)),
            _ => {
                if let Some(op) = BinOp::from_name(w) {
                    let (lhs, rhs) = self.two(c)?;
                    Value::Binary { op, lhs, rhs }
                } else if let Some(op) = UnOp::from_name(w) {
                    c.need("(")?;
                    let operand = self.operand(c)?;
                    c.need(")")?;
                    Value::Unary { op, operand }
                } else {
                    *c = start;
                    c.ws();
                    return Err(c.err(ParseWhat::Expected("a value")));
                }
            }
        };
        Ok(Row::Kind(StmtKind::Assign { dest, value: v }))
    }

    fn assert_kind(c: &mut Cur<'_>) -> Result<AssertKind, ParseError> {
        let w = c.word();
        let k = match w {
            "overflow_neg" => Some(AssertKind::OverflowNeg),
            "bounds" => Some(AssertKind::Bounds),
            "div_zero" => Some(AssertKind::DivisionByZero),
            "rem_zero" => Some(AssertKind::RemainderByZero),
            "resumed_after_return" => Some(AssertKind::ResumedAfterReturn),
            "resumed_after_panic" => Some(AssertKind::ResumedAfterPanic),
            _ => w.strip_prefix("overflow_").and_then(BinOp::from_name).map(AssertKind::Overflow),
        };
        k.ok_or_else(|| c.err(ParseWhat::Expected("an assert kind")))
    }

    fn row(&mut self, c: &mut Cur<'_>) -> Result<Row, ParseError> {
        c.ws();
        if c.rest().starts_with('_') {
            let dest = self.place(c)?;
            c.need("=")?;
            return self.value(c, dest);
        }
        let k = match c.word() {
            "live" => StmtKind::StorageLive(Self::local(c)?),
            "dead" => StmtKind::StorageDead(Self::local(c)?),
            "activate" => StmtKind::Activate(Loan(c.prefixed("L")?)),
            "fakeread" => StmtKind::FakeRead(self.place(c)?),
            "setdiscr" => {
                let place = self.place(c)?;
                if !c.keyword("variant") {
                    return Err(c.err(ParseWhat::Expected("variant")));
                }
                StmtKind::SetDiscriminant { place, variant: c.num()? }
            }
            "deinit" => StmtKind::Deinit(self.place(c)?),
            "goto" => StmtKind::Goto(Self::block(c)?),
            "switch" => {
                let on = self.operand(c)?;
                c.need("[")?;
                self.cases.clear();
                if !c.eat("]") {
                    loop {
                        c.ws();
                        let v = c.digits(10)?;
                        c.need("->")?;
                        let t = Self::block(c)?;
                        self.cases.push((v, t));
                        if c.eat("]") {
                            break;
                        }
                        c.need(",")?;
                    }
                }
                let cases = self.b.cases(&self.cases);
                if !c.keyword("else") {
                    return Err(c.err(ParseWhat::Expected("else")));
                }
                StmtKind::Switch { on, cases, otherwise: Self::block(c)? }
            }
            "call" => {
                let callee = match c.word() {
                    "fn" => Self::item(c).map(|(key, args)| Callee::FnItem { key, args })?,
                    "method" => Self::item(c).map(|(key, args)| Callee::Method { key, args })?,
                    "intrinsic" => Self::item(c).map(|(key, args)| Callee::Intrinsic { key, args })?,
                    "value" => Callee::Value(self.operand(c)?),
                    _ => return Err(c.err(ParseWhat::Expected("a callee"))),
                };
                let callee = self.b.callee(callee);
                let args = self.ops(c)?;
                let dest = match c.eat("->") {
                    true => Some(self.place(c)?),
                    false => None,
                };
                let target = Self::then(c)?;
                StmtKind::Call { callee, args, dest, target, unwind: Self::unwind(c)? }
            }
            "drop" => {
                let place = self.place(c)?;
                let target = Self::need_then(c)?;
                StmtKind::Drop { place, target, unwind: Self::unwind(c)? }
            }
            "assert" => {
                let cond = self.operand(c)?;
                c.need("==")?;
                let expected = match c.word() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(c.err(ParseWhat::Expected("true or false"))),
                };
                c.need(",")?;
                let kind = Self::assert_kind(c)?;
                let msg = self.ops(c)?;
                let target = Self::need_then(c)?;
                StmtKind::Assert { cond, expected, kind, msg, target, unwind: Self::unwind(c)? }
            }
            "suspend" => {
                if !c.keyword("resume") {
                    return Err(c.err(ParseWhat::Expected("resume")));
                }
                let resume = Self::block(c)?;
                if !c.keyword("cancel") {
                    return Err(c.err(ParseWhat::Expected("cancel")));
                }
                StmtKind::Suspend { resume, cancel: Self::block(c)? }
            }
            "return" => StmtKind::Return,
            "unreachable" => StmtKind::Unreachable,
            "resume" => StmtKind::Resume,
            "terminate" => StmtKind::Terminate,
            "asm" => {
                let asm = c.num()?;
                let operands = self.ops(c)?;
                let target = Self::then(c)?;
                StmtKind::InlineAsm { asm, operands, target, unwind: Self::unwind(c)? }
            }
            _ => return Err(c.err(ParseWhat::Expected("a statement"))),
        };
        Ok(Row::Kind(k))
    }

    fn stmt(&mut self, c: &mut Cur<'_>) -> Result<(), ParseError> {
        let row = self.row(c)?;
        let span = match c.eat("@") {
            true => {
                let lo = c.u32()?;
                if !c.tight("..") {
                    return Err(c.err(ParseWhat::Expected("..")));
                }
                Span { lo, hi: c.u32()? }
            }
            false => Span::default(),
        };
        match row {
            Row::Borrow { dest, kind, place, loan, origin } => {
                let want = self.b.body().view().loans();
                Self::next_id(c, Table::Loans, want, loan.0)?;
                self.b.borrow(dest, kind, place, origin, span);
            }
            Row::Kind(k) if k.is_terminator() => {
                self.b.terminate(k, span);
            }
            Row::Kind(k) => {
                self.b.push(k, span);
            }
        }
        if c.eat(";") {
            loop {
                let from = Self::origin(c)?;
                c.need(":")?;
                let into = Self::origin(c)?;
                self.b.subset(from, into);
                if !c.eat(",") {
                    break;
                }
            }
        }
        c.done()
    }
}
