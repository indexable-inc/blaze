//! LIR to LLVM IR text (D2, as changed by D100 points 7 and 9): one module holding the function,
//! its offered callees' bodies, and a declaration for everything else it names, with blaze's
//! facts SUPPLIED as attributes. Nothing is inferred: a callee given as facts only is a
//! declaration whose attributes are exactly its `CalleeFacts` (`declare i64 @bz..(i64) nounwind
//! readonly`); an offered callee in LIR is an `available_externally` definition here; an
//! offered callee in optimized bitcode is handed to [`crate::bitcode`] as a [`Linked`], and
//! linked in `available_externally` there.
//!
//! LIR's semantics are kept where LLVM's differ: a shift masks its amount, division by zero and
//! `MIN / -1` trap (each a branch to a trap) unless the instruction promises otherwise
//! (`nonzero`, `nsw`), `fpto*i` saturates (`llvm.fpto*i.sat`), `uninit` is `undef`. Promises
//! (`nsw`, `nuw`, `exact`, `disjoint`, `inbounds`, fast-math, alignment, `!nonnull`,
//! `!noundef`, `!invariant.load`, parameter facts) are given to LLVM; the promises and hints
//! LLVM is not given are dropped by LIR's own rule (a promise or hint may be dropped):
//! `slot_live`/`slot_dead`, a load's range and pointee alignment, `nontemporal`, `nuw` on
//! `ptr_add`. Every form not lowered is refused BY NAME (`LowerError::Refused`): coroutines
//! (`suspend`, `cancelled`), `asm!`, target intrinsics, thread-locals, `stack_pointer`, scalable
//! vectors, masked and gathered memory, `swizzle`, `bitmask`, the float reductions with a start
//! value. Unwinding (`invoke`, landing blocks, `resume`, the personality) is lowered by the child
//! module lower_eh.rs (UNWIND.md 3.8, P8a); a landing block reached by anything but an unwind edge
//! is `LowerError::Landing`.
//!
//! Names: every symbol is `bz` and 64 hex digits (names.rs); a C name is itself. Values are
//! `%v<index>`, blocks `%b<index>`, an edge carrying arguments its own block `%e<inst>_<k>`
//! (so a phi names one predecessor per edge), stack slots `%ss<index>` in the entry block.
//! The text is the same bytes for the same inputs in any order: offered bodies, declarations
//! and linked bitcode are sorted by symbol.

use core::fmt::{self, Write as _};

use blaze_code::record::Symbol;
use blaze_lir::flags::Ordering;
use blaze_lir::func::{
    Block, BlockKind, Conv, DataId, Ext, FnAttrs, Inline, MemAttrsRec, NONE, ParamFactsRec, ParamKind, ParamRec,
    RefId, RefKind, Sig, SigId, SigRec, Value,
};
use blaze_lir::op::Target;
use blaze_lir::ty::{Form, Scalar};
use blaze_lir::{Flags, Op, Ty, View};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_tier::{Named, Names, Outside};

use crate::compile::Linked;
use crate::names::{ir_name, ir_name_str, symbol_in};

// Unwinding's forms (UNWIND.md 3.8, P8a): a child module, so it writes through `Emit`.
#[path = "lower_eh.rs"]
mod eh;

/// A callee offered to the inliner (D100 point 9): in LIR (lowered here as an
/// `available_externally` body) or in its optimized bitcode (linked by [`crate::bitcode`]).
pub enum Offered<'v> {
    Lir { view: View<'v>, names: &'v dyn Names, symbol: Symbol },
    Bitcode { bytes: &'v [u8], symbol: Symbol },
}

/// The facts a callee's body would have given, from blaze's fact records, supplied as its
/// declaration's attributes (D100 point 7 (b)). Each is read by content: a callee edited
/// without changing these invalidates nobody.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CalleeFacts {
    pub symbol: Symbol,
    pub nounwind: bool,
    pub readonly: bool,
    pub readnone: bool,
    pub noalias_ret: bool,
}

/// The module: its text and the bitcode bodies to link into it.
pub struct Lowered<'a, 'v> {
    pub ir: AString<'a>,
    /// Sorted by symbol.
    pub linked: AVec<'a, Linked<'v>>,
}

/// Why a function was not lowered: each names the body (its symbol) and, where it is one
/// instruction's, the instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LowerError {
    /// A form this lowering does not handle, by name.
    Refused { body: Symbol, inst: Option<u32>, what: &'static str },
    /// The body's [`Names`] has no name for `of`.
    NoName { body: Symbol, of: Outside },
    /// A C name that is not printable as an LLVM name, or that has the `bz` symbol form.
    CName { body: Symbol, what: &'static str },
    /// A symbol offered twice, or offered and also the function itself.
    Offered { symbol: Symbol, what: &'static str },
    /// Two `CalleeFacts` rows for one symbol.
    Facts { symbol: Symbol },
    /// Landing block `block` reached by `by`: only an invoke's unwind edge may reach one (LIR's
    /// rule, blaze-lir verify.rs, and LLVM's for a block that starts with `landingpad`).
    Landing { body: Symbol, block: u32, by: &'static str },
    /// Writing the text failed (an allocator's refusal surfaces as `fmt::Error`).
    Text,
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            LowerError::Refused { body, inst: Some(inst), what } => {
                write!(f, "LIR to LLVM: {body} instruction {inst}: {what} is not lowered")
            }
            LowerError::Refused { body, inst: None, what } => write!(f, "LIR to LLVM: {body}: {what} is not lowered"),
            LowerError::NoName { body, of } => write!(f, "LIR to LLVM: {body} has no name for {of}"),
            LowerError::CName { body, what } => write!(f, "LIR to LLVM: {body} names a C name that {what}"),
            LowerError::Offered { symbol, what } => write!(f, "LIR to LLVM: the offered callee {symbol} is {what}"),
            LowerError::Facts { symbol } => write!(f, "LIR to LLVM: two fact rows for {symbol}"),
            LowerError::Landing { body, block, by } => {
                write!(f, "LIR to LLVM: {body} block {block}: a landing block reached by {by}; only an unwind edge reaches one")
            }
            LowerError::Text => f.write_str("LIR to LLVM: writing the module's text failed"),
        }
    }
}

impl core::error::Error for LowerError {}

impl From<fmt::Error> for LowerError {
    fn from(_: fmt::Error) -> LowerError {
        LowerError::Text
    }
}

/// Lowers `func` (named `symbol`, its outside references named by `names`) with its offered
/// callees and the facts of every callee into one module.
pub fn lower<'a, 'v>(
    func: View<'_>,
    names: &dyn Names,
    symbol: Symbol,
    offered: &[Offered<'v>],
    facts: &[CalleeFacts],
    alloc: Alloc<'a>,
) -> Result<Lowered<'a, 'v>, LowerError> {
    // Every symbol defined or linked here, sorted: one each.
    let mut defined: AVec<'a, Symbol> = Vec::new_in(alloc);
    defined.push(symbol);
    let mut lir: AVec<'a, (Symbol, usize)> = Vec::new_in(alloc);
    let mut linked: AVec<'a, Linked<'v>> = Vec::new_in(alloc);
    for (at, one) in offered.iter().enumerate() {
        match *one {
            Offered::Lir { symbol, .. } => {
                lir.push((symbol, at));
                defined.push(symbol);
            }
            Offered::Bitcode { bytes, symbol } => linked.push(Linked { symbol, bytes }),
        }
    }
    let mut all: AVec<'a, Symbol> = Vec::new_in(alloc);
    all.extend_from_slice(&defined);
    all.extend(linked.iter().map(|it| it.symbol));
    all.sort_unstable();
    if let Some(pair) = all.windows(2).find(|pair| pair[0] == pair[1]) {
        let what = if pair[0] == symbol { "the function itself" } else { "offered twice" };
        return Err(LowerError::Offered { symbol: pair[0], what });
    }
    defined.sort_unstable();
    lir.sort_unstable_by_key(|it| it.0);
    linked.sort_unstable_by_key(|it| it.symbol);
    let mut facts_sorted: AVec<'a, CalleeFacts> = Vec::new_in(alloc);
    facts_sorted.extend_from_slice(facts);
    facts_sorted.sort_unstable_by_key(|it| it.symbol);
    if let Some(pair) = facts_sorted.windows(2).find(|pair| pair[0].symbol == pair[1].symbol) {
        return Err(LowerError::Facts { symbol: pair[0].symbol });
    }

    let mut ir = String::new_in(alloc);
    let mut decls: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let cx = Cx { defined: &defined, facts: &facts_sorted, alloc };
    Emit::new(func, names, symbol, 0, "", &cx, &mut ir, &mut decls).body()?;
    for (index, &(sym, at)) in lir.iter().enumerate() {
        let Offered::Lir { view, names, .. } = &offered[at] else { continue };
        Emit::new(*view, *names, sym, index as u32 + 1, "available_externally ", &cx, &mut ir, &mut decls).body()?;
    }
    decls.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    decls.dedup_by(|a, b| a.as_str() == b.as_str());
    for line in decls.iter() {
        ir.push_str(line.as_str());
        ir.push('\n');
    }
    // Metadata: the empty node, and the two expectations' branch weights.
    ir.push_str("!0 = !{}\n!1 = !{!\"branch_weights\", i32 2000, i32 1}\n!2 = !{!\"branch_weights\", i32 1, i32 2000}\n");
    Ok(Lowered { ir, linked })
}

struct Cx<'c, 'a> {
    defined: &'c [Symbol],
    facts: &'c [CalleeFacts],
    alloc: Alloc<'a>,
}

impl Cx<'_, '_> {
    fn facts_of(&self, symbol: Symbol) -> Option<CalleeFacts> {
        self.facts.binary_search_by_key(&symbol, |it| it.symbol).ok().map(|at| self.facts[at])
    }
}

/// An argument of an intrinsic call.
#[derive(Clone, Copy)]
enum Arg<'s> {
    /// A LIR value, with its type.
    V(Value),
    /// A literal: its type and its text.
    Lit(&'s str, &'s str),
    /// A helper local `%v<inst>.<suffix>` of type `Ty`.
    Tmp(Ty, u32, &'s str),
}

struct Emit<'f, 'n, 'c, 'o, 'a> {
    view: View<'f>,
    names: &'n dyn Names,
    body: Symbol,
    index: u32,
    linkage: &'static str,
    cx: &'c Cx<'c, 'a>,
    out: &'o mut AString<'a>,
    decls: &'o mut AVec<'a, AString<'a>>,
}

impl<'f, 'n, 'c, 'o, 'a> Emit<'f, 'n, 'c, 'o, 'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        view: View<'f>,
        names: &'n dyn Names,
        body: Symbol,
        index: u32,
        linkage: &'static str,
        cx: &'c Cx<'c, 'a>,
        out: &'o mut AString<'a>,
        decls: &'o mut AVec<'a, AString<'a>>,
    ) -> Self {
        Emit { view, names, body, index, linkage, cx, out, decls }
    }

    fn refuse(&self, inst: Option<usize>, what: &'static str) -> LowerError {
        LowerError::Refused { body: self.body, inst: inst.map(|it| it as u32), what }
    }

    // ---- The body.

    fn body(&mut self) -> Result<(), LowerError> {
        let view = self.view;
        if view.block_starts.is_empty() {
            return Err(self.refuse(None, "a function with no block"));
        }
        self.data()?;
        let sigs = self.sigs_of_refs()?;
        self.ref_decls(&sigs)?;

        let sig = view.sig(SigId(view.head.sig));
        let params = view.params(Block(0));
        if params.len() != sig.params.len() {
            return Err(self.refuse(None, "an entry block whose parameters are not the signature's"));
        }
        // The header.
        write!(self.out, "define {}", self.linkage)?;
        self.conv(sig.conv, None)?;
        self.ret_attrs(sig)?;
        self.ret_text(sig)?;
        let name = ir_name(self.body);
        write!(self.out, " @{}(", ir_name_str(&name))?;
        for (j, inst) in params.clone().enumerate() {
            if j > 0 {
                self.out.push_str(", ");
            }
            self.ty(view.ty(Value(inst as u32)))?;
            self.param_attrs(&sig.params[j])?;
            write!(self.out, " %v{inst}")?;
        }
        if sig.bits & SigRec::VARIADIC != 0 {
            return Err(self.refuse(None, "a variadic definition"));
        }
        self.out.push(')');
        self.fn_attrs()?;
        self.personality()?;
        self.out.push_str(" {\nentry:\n");
        for (k, slot) in view.slots.iter().enumerate() {
            writeln!(self.out, "  %ss{k} = alloca [{} x i8], align {}", slot.size, 1u64 << slot.align)?;
        }
        self.out.push_str("  br label %b0\n");

        // Edges carrying arguments, sorted by their target: the phis' predecessors.
        let mut edges: AVec<'a, (u32, u32, u32)> = Vec::new_in(self.cx.alloc);
        let mut targets: AVec<'a, Target<'f>> = Vec::new_in(self.cx.alloc);
        for b in 0..view.blocks() {
            let term = view.terminator(Block(b as u32));
            targets.clear();
            self.targets(term, &mut targets);
            for (k, target) in targets.iter().enumerate() {
                if target.block.0 == 0 {
                    return Err(self.refuse(Some(term), "a branch to the entry block"));
                }
                if view.block_kind(target.block).is_landing() {
                    return Err(self.landing(target.block, "a jump, branch or switch"));
                }
                let wanted = view.params(target.block).len();
                if target.args.len() != wanted {
                    return Err(self.refuse(Some(term), "an edge whose arguments are not its target's parameters"));
                }
                if !target.args.is_empty() {
                    edges.push((target.block.0, term as u32, k as u32));
                }
            }
        }
        edges.sort_unstable();

        for b in 0..view.blocks() {
            let block = Block(b as u32);
            let kind = view.block_kind(block);
            writeln!(self.out, "b{b}:")?;
            if kind.is_landing() {
                // Its one parameter comes from the landing pad, never from an edge.
                self.landing_head(block, kind)?;
            } else if b > 0 {
                self.phis(block, &edges)?;
            }
            let range = view.block(block);
            let first = view.params(block).end;
            for inst in first..range.end {
                self.inst(inst)?;
            }
        }
        for &(target, term, k) in edges.iter() {
            writeln!(self.out, "e{term}_{k}:\n  br label %b{target}")?;
        }
        self.out.push_str("}\n");
        Ok(())
    }

    fn phis(&mut self, block: Block, edges: &[(u32, u32, u32)]) -> Result<(), LowerError> {
        let view = self.view;
        let from = edges.partition_point(|it| it.0 < block.0);
        let to = edges.partition_point(|it| it.0 <= block.0);
        let mut targets: AVec<'a, Target<'f>> = Vec::new_in(self.cx.alloc);
        for (j, param) in view.params(block).enumerate() {
            let ty = view.ty(Value(param as u32));
            if from == to {
                // No edge reaches the block: its parameters have no value.
                write!(self.out, "  %v{param} = freeze ")?;
                self.ty(ty)?;
                self.out.push_str(" poison\n");
                continue;
            }
            write!(self.out, "  %v{param} = phi ")?;
            self.ty(ty)?;
            for (n, &(_, term, k)) in edges[from..to].iter().enumerate() {
                targets.clear();
                self.targets(term as usize, &mut targets);
                let arg = targets[k as usize].args.get(j);
                self.out.push_str(if n == 0 { " [ " } else { ", [ " });
                self.val(arg)?;
                write!(self.out, ", %e{term}_{k} ]")?;
            }
            self.out.push('\n');
        }
        Ok(())
    }

    /// The targets of terminator `inst`, in operand order (a switch's default first).
    fn targets(&self, inst: usize, out: &mut AVec<'a, Target<'f>>) {
        let mut o = self.view.operands(inst);
        match self.view.op(inst) {
            Op::Jump => out.push(o.target()),
            Op::Br => {
                o.value();
                out.push(o.target());
                out.push(o.target());
            }
            Op::Switch => {
                let scrutinee = o.value();
                out.push(o.target());
                out.extend(o.cases(scrutinee).map(|(_, target)| target));
            }
            _ => {}
        }
    }

    fn label(&mut self, inst: usize, k: usize, target: &Target<'_>) -> Result<(), LowerError> {
        if target.args.is_empty() {
            write!(self.out, "label %b{}", target.block.0)?;
        } else {
            write!(self.out, "label %e{inst}_{k}")?;
        }
        Ok(())
    }

    // ---- Data, references and declarations.

    /// The body's own data, one private constant each: bytes, with each address a pointer.
    fn data(&mut self) -> Result<(), LowerError> {
        let view = self.view;
        for (d, rec) in view.datas.iter().enumerate() {
            let id = DataId(d as u32);
            let bytes = view.data_bytes_of(id);
            let relocs = view.data_relocs_of(id);
            write!(self.out, "@d{}_{d} = private unnamed_addr constant <{{ ", self.index)?;
            let mut at = 0usize;
            let mut first = true;
            let ptr_bytes = (view.head.ptr_bits / 8) as usize;
            let mut pieces: AString<'a> = String::new_in(self.cx.alloc);
            let mut values: AString<'a> = String::new_in(self.cx.alloc);
            for reloc in relocs {
                let off = reloc.offset as usize;
                if off < at || off + ptr_bytes > bytes.len() {
                    return Err(self.refuse(None, "data whose addresses overlap or pass its end"));
                }
                if off > at {
                    piece_bytes(&mut pieces, &mut values, &bytes[at..off], &mut first)?;
                }
                if !first {
                    pieces.push_str(", ");
                    values.push_str(", ");
                }
                first = false;
                pieces.push_str("ptr");
                values.push_str("ptr getelementptr (i8, ptr ");
                let saved = core::mem::replace(self.out, String::new_in(self.cx.alloc));
                let named = self.ref_name(RefId(reloc.target), None);
                let text = core::mem::replace(self.out, saved);
                named?;
                values.push_str(text.as_str());
                write!(values, ", i64 {})", reloc.addend)?;
                at = off + ptr_bytes;
            }
            if at < bytes.len() || bytes.is_empty() {
                piece_bytes(&mut pieces, &mut values, &bytes[at..], &mut first)?;
            }
            self.out.push_str(pieces.as_str());
            self.out.push_str(" }> <{ ");
            self.out.push_str(values.as_str());
            writeln!(self.out, " }}>, align {}", 1u64 << rec.align.min(29))?;
        }
        Ok(())
    }

    /// Per row of `refs`, the signature of the first call through it (`NONE` for none).
    fn sigs_of_refs(&self) -> Result<AVec<'a, u32>, LowerError> {
        let view = self.view;
        let mut sigs: AVec<'a, u32> = Vec::new_in(self.cx.alloc);
        sigs.resize(view.refs.len(), NONE);
        for inst in 0..view.len() {
            if matches!(view.op(inst), Op::Call | Op::TailCall | Op::Invoke) {
                let mut o = view.operands(inst);
                let r = o.reference();
                let sig = o.sig();
                if sigs[r.index()] == NONE {
                    sigs[r.index()] = sig.0;
                }
            }
        }
        Ok(sigs)
    }

    /// A declaration for every outside function and static the body names, with the facts of
    /// each callee as its attributes (and nothing else).
    fn ref_decls(&mut self, sigs: &[u32]) -> Result<(), LowerError> {
        let view = self.view;
        for (r, rec) in view.refs.iter().enumerate() {
            let kind = RefKind::from_word(rec.kind);
            let mut line: AString<'a> = String::new_in(self.cx.alloc);
            let saved = core::mem::replace(self.out, String::new_in(self.cx.alloc));
            let result = self.decl_of(RefId(r as u32), kind, sigs[r]);
            let text = core::mem::replace(self.out, saved);
            if result? {
                line.push_str(text.as_str());
                self.decls.push(line);
            }
        }
        Ok(())
    }

    /// Writes ref `r`'s declaration into `self.out`; `false` when it needs none (own data, a
    /// function defined in this module).
    fn decl_of(&mut self, r: RefId, kind: Option<RefKind>, sig: u32) -> Result<bool, LowerError> {
        match kind {
            Some(RefKind::Data) => Ok(false),
            Some(RefKind::Tls) => Err(self.refuse(None, "a thread-local")),
            None => Err(self.refuse(None, "a reference of an unknown kind")),
            Some(RefKind::Static | RefKind::Vtable) => {
                self.ref_name(r, None)?;
                self.out.push_str(" = external global i8");
                Ok(true)
            }
            Some(RefKind::Func) => {
                let mut name: AString<'a> = String::new_in(self.cx.alloc);
                let saved = core::mem::replace(self.out, String::new_in(self.cx.alloc));
                let symbol = self.ref_name(r, None);
                let text = core::mem::replace(self.out, saved);
                let symbol = symbol?;
                if self.cx.defined.binary_search(&symbol).is_ok() {
                    return Ok(false);
                }
                name.push_str(text.as_str());
                let facts = self.cx.facts_of(symbol);
                self.out.push_str("declare ");
                if sig == NONE {
                    write!(self.out, "void {}()", name.as_str())?;
                } else {
                    let sig = self.view.sig(SigId(sig));
                    self.conv(sig.conv, None)?;
                    if facts.is_some_and(|it| it.noalias_ret) {
                        if sig.rets.len() != 1 || !Ty(sig.rets[0].ty).is_ptr() {
                            return Err(self.refuse(None, "a `noalias` return fact on a function not returning one pointer"));
                        }
                        self.out.push_str("noalias ");
                    }
                    self.ret_attrs(sig)?;
                    self.ret_text(sig)?;
                    write!(self.out, " {}(", name.as_str())?;
                    for (j, param) in sig.params.iter().enumerate() {
                        if j > 0 {
                            self.out.push_str(", ");
                        }
                        self.ty(Ty(param.ty))?;
                        self.param_attrs(param)?;
                    }
                    if sig.bits & SigRec::VARIADIC != 0 {
                        self.out.push_str(if sig.params.is_empty() { "..." } else { ", ..." });
                    }
                    self.out.push(')');
                }
                if let Some(facts) = facts {
                    self.facts_attrs(facts)?;
                }
                Ok(true)
            }
        }
    }

    /// Writes `@name` of ref `r` (own data by its global); returns the symbol it names (a C
    /// name's is `Symbol::of_name`; own data's is the body's own).
    fn ref_name(&mut self, r: RefId, inst: Option<usize>) -> Result<Symbol, LowerError> {
        let rec = self.view.reference(r);
        let of = match RefKind::from_word(rec.kind) {
            Some(RefKind::Func) => Outside::Func(rec.place),
            Some(RefKind::Static) => Outside::Static(rec.place),
            Some(RefKind::Vtable) => Outside::Vtable(rec.place),
            Some(RefKind::Data) => {
                write!(self.out, "@d{}_{}", self.index, rec.place)?;
                return Ok(self.body);
            }
            Some(RefKind::Tls) => return Err(self.refuse(inst, "a thread-local")),
            None => return Err(self.refuse(inst, "a reference of an unknown kind")),
        };
        match self.names.name(of) {
            None => Err(LowerError::NoName { body: self.body, of }),
            Some(Named::Symbol(symbol)) => {
                write!(self.out, "@{}", ir_name_str(&ir_name(symbol)))?;
                Ok(symbol)
            }
            Some(Named::C(name)) => {
                if symbol_in(name.as_bytes()).is_some() {
                    return Err(LowerError::CName { body: self.body, what: "has the `bz` symbol form" });
                }
                if name.is_empty() || name.bytes().any(|b| b == b'"' || b == b'\\' || !(0x20..0x7f).contains(&b)) {
                    return Err(LowerError::CName { body: self.body, what: "is empty or holds a quote, backslash or control byte" });
                }
                write!(self.out, "@\"{name}\"")?;
                Ok(Symbol::of_name(name))
            }
        }
    }

    // ---- Attributes and types.

    fn facts_attrs(&mut self, facts: CalleeFacts) -> Result<(), LowerError> {
        if facts.nounwind {
            self.out.push_str(" nounwind");
        }
        if facts.readnone {
            self.out.push_str(" readnone");
        } else if facts.readonly {
            self.out.push_str(" readonly");
        }
        Ok(())
    }

    fn fn_attrs(&mut self) -> Result<(), LowerError> {
        let attrs = FnAttrs(self.view.head.attrs);
        for &(bit, name) in &FnAttrs::NAMED {
            if attrs.0 & bit != 0 {
                write!(self.out, " {name}")?;
            }
        }
        match attrs.inline() {
            Inline::None => {}
            Inline::Never => self.out.push_str(" noinline"),
            other => write!(self.out, " {}", other.name())?,
        }
        if attrs.0 & FnAttrs::OPT_NONE != 0 && attrs.inline() != Inline::Never {
            self.out.push_str(" noinline");
        }
        // The samplers walk frame records (D100 point 4); every tier unwinds (point 10).
        self.out.push_str(" uwtable \"frame-pointer\"=\"all\"");
        if let Some(facts) = self.cx.facts_of(self.body)
            && self.index > 0
        {
            self.facts_attrs(facts)?;
        }
        if !self.view.features.is_empty() {
            let features = core::str::from_utf8(self.view.features).map_err(|_| self.refuse(None, "target features that are not UTF-8"))?;
            write!(self.out, " \"target-features\"=\"{features}\"")?;
        }
        Ok(())
    }

    fn conv(&mut self, conv: u8, inst: Option<usize>) -> Result<(), LowerError> {
        match Conv::from_byte(conv) {
            Some(Conv::C) => {}
            Some(Conv::PreserveMost) => self.out.push_str("preserve_mostcc "),
            Some(Conv::Aapcs) => self.out.push_str("arm_aapcscc "),
            Some(Conv::SysV64) => self.out.push_str("x86_64_sysvcc "),
            Some(Conv::Win64) => self.out.push_str("win64cc "),
            Some(Conv::Wasm) => return Err(self.refuse(inst, "the wasm calling convention")),
            None => return Err(self.refuse(inst, "an unknown calling convention")),
        }
        Ok(())
    }

    fn ret_text(&mut self, sig: Sig<'_>) -> Result<(), LowerError> {
        match sig.rets {
            [] => self.out.push_str("void"),
            [one] => self.ty(Ty(one.ty))?,
            many => {
                self.out.push_str("{ ");
                for (j, ret) in many.iter().enumerate() {
                    if j > 0 {
                        self.out.push_str(", ");
                    }
                    self.ty(Ty(ret.ty))?;
                }
                self.out.push_str(" }");
            }
        }
        Ok(())
    }

    /// A single return's attributes, before its type.
    fn ret_attrs(&mut self, sig: Sig<'_>) -> Result<(), LowerError> {
        let [ret] = sig.rets else { return Ok(()) };
        self.ext(ret.ext)?;
        if let Some(facts) = self.view.facts(ret) {
            let ty = Ty(ret.ty);
            for (bit, name) in [
                (ParamFactsRec::NOALIAS, "noalias"),
                (ParamFactsRec::NONNULL, "nonnull"),
                (ParamFactsRec::NOUNDEF, "noundef"),
            ] {
                if facts.bits & bit != 0 {
                    write!(self.out, "{name} ")?;
                }
            }
            self.deref_align_range(facts, ty)?;
            self.out.push(' ');
        }
        Ok(())
    }

    fn param_attrs(&mut self, param: &ParamRec) -> Result<(), LowerError> {
        if let Some(Ext::Zext | Ext::Sext) = Ext::from_byte(param.ext) {
            self.out.push(' ');
            self.ext(param.ext)?;
        }
        let facts = self.view.facts(param);
        match ParamKind::from_byte(param.kind) {
            Some(ParamKind::Value) => {}
            Some(kind @ (ParamKind::Sret | ParamKind::Byval)) => {
                let size = facts.map_or(0, |it| it.size);
                write!(self.out, " {}([{size} x i8])", kind.name())?;
            }
            None => return Err(self.refuse(None, "a parameter of an unknown kind")),
        }
        if let Some(facts) = facts {
            for (bit, name) in [
                (ParamFactsRec::NONNULL, "nonnull"),
                (ParamFactsRec::NOALIAS, "noalias"),
                (ParamFactsRec::READONLY, "readonly"),
                (ParamFactsRec::NOUNDEF, "noundef"),
                (ParamFactsRec::CAPTURES_NONE, "captures(none)"),
                (ParamFactsRec::WRITABLE, "writable"),
                (ParamFactsRec::DEAD_ON_UNWIND, "dead_on_unwind"),
            ] {
                if facts.bits & bit != 0 {
                    write!(self.out, " {name}")?;
                }
            }
            self.out.push(' ');
            self.deref_align_range(facts, Ty(param.ty))?;
        }
        Ok(())
    }

    fn ext(&mut self, ext: u8) -> Result<(), LowerError> {
        match Ext::from_byte(ext) {
            Some(Ext::None) => {}
            Some(Ext::Zext) => self.out.push_str("zeroext "),
            Some(Ext::Sext) => self.out.push_str("signext "),
            None => return Err(self.refuse(None, "an unknown extension")),
        }
        Ok(())
    }

    /// `dereferenceable`, `align` and `range` of a parameter or return's facts (each followed
    /// by a space).
    fn deref_align_range(&mut self, facts: &ParamFactsRec, ty: Ty) -> Result<(), LowerError> {
        if facts.deref > 0 {
            let name = if facts.bits & ParamFactsRec::DEREF_OR_NULL != 0 { "dereferenceable_or_null" } else { "dereferenceable" };
            write!(self.out, "{name}({}) ", facts.deref)?;
        }
        if facts.align > 0 {
            write!(self.out, "align {} ", 1u64 << facts.align.min(32))?;
        }
        if let Some(range) = facts.range.checked_sub(1).and_then(|row| self.view.ranges.get(row as usize))
            && ty.is_int()
            && ty.is_scalar()
        {
            let bits = ty.bits(self.view.head.ptr_bits);
            let mask = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
            let lo = range.start & mask;
            let hi = range.end.wrapping_add(1) & mask;
            if lo != hi {
                write!(self.out, "range(i{bits} {lo}, {hi}) ")?;
            }
        }
        Ok(())
    }

    fn ty(&mut self, ty: Ty) -> Result<(), LowerError> {
        match ty.form() {
            Form::Scalar => self.out.push_str(scalar(ty.elem())),
            Form::Vector => write!(self.out, "<{} x {}>", ty.lanes(), scalar(ty.elem()))?,
            Form::Scalable => return Err(self.refuse(None, "a scalable vector")),
            Form::Compound => {
                self.out.push_str("{ ");
                for (j, &member) in self.view.members(ty).iter().enumerate() {
                    if j > 0 {
                        self.out.push_str(", ");
                    }
                    let member = Ty(member);
                    match member.form() {
                        Form::Scalar => self.out.push_str(scalar(member.elem())),
                        Form::Vector => write!(self.out, "<{} x {}>", member.lanes(), scalar(member.elem()))?,
                        Form::Scalable | Form::Compound => {
                            return Err(self.refuse(None, "a compound holding a compound or scalable vector"));
                        }
                    }
                }
                self.out.push_str(" }");
            }
        }
        Ok(())
    }

    /// An intrinsic's name suffix for `ty`: `i32`, `f64`, `p0`, `v4i32`.
    fn mangle(&self, out: &mut AString<'a>, ty: Ty) -> Result<(), LowerError> {
        let elem = match ty.elem() {
            Scalar::Ptr => "p0",
            Scalar::F16 => "f16",
            Scalar::F32 => "f32",
            Scalar::F64 => "f64",
            Scalar::F128 => "f128",
            other => scalar(other),
        };
        match ty.form() {
            Form::Scalar => out.push_str(elem),
            Form::Vector => write!(out, "v{}{elem}", ty.lanes())?,
            Form::Scalable | Form::Compound => return Err(self.refuse(None, "an intrinsic over a compound or scalable type")),
        }
        Ok(())
    }

    // ---- Operands.

    /// Writes value `v`: a constant, an address or a slot inline, else `%v<index>`.
    fn val(&mut self, v: Value) -> Result<(), LowerError> {
        let view = self.view;
        let inst = v.index();
        let ty = view.ty(v);
        match view.op(inst) {
            Op::Iconst => {
                let imm = view.operands(inst).imm();
                if ty.is_vector() {
                    self.out.push_str("splat (");
                    self.out.push_str(scalar(ty.elem()));
                    self.out.push(' ');
                    self.int_literal(ty.lane(), imm)?;
                    self.out.push(')');
                } else {
                    self.int_literal(ty, imm)?;
                }
            }
            Op::IconstWide => {
                let words = view.operands(inst).constant();
                write!(self.out, "{}", words_value(words))?;
            }
            Op::Fconst => {
                let bits = view.operands(inst).imm();
                write!(self.out, "bitcast (i{} {bits} to ", ty.bits(64))?;
                self.ty(ty)?;
                self.out.push(')');
            }
            Op::FconstWide => {
                let words = view.operands(inst).constant();
                write!(self.out, "bitcast (i{} {} to ", ty.bits(64), words_value(words))?;
                self.ty(ty)?;
                self.out.push(')');
            }
            Op::Vconst => {
                let words = view.operands(inst).constant();
                let len = (ty.bits(64) / 8) as usize;
                write!(self.out, "bitcast (<{len} x i8> <")?;
                for n in 0..len {
                    let byte = (words[n / 4] >> (8 * (n % 4))) & 0xff;
                    write!(self.out, "{}i8 {byte}", if n == 0 { "" } else { ", " })?;
                }
                self.out.push_str("> to ");
                self.ty(ty)?;
                self.out.push(')');
            }
            Op::Null => self.out.push_str("null"),
            Op::Uninit => self.out.push_str("undef"),
            Op::FuncAddr | Op::DataAddr => {
                let r = view.operands(inst).reference();
                self.ref_name(r, Some(inst))?;
            }
            Op::SlotAddr => {
                let slot = view.operands(inst).slot();
                write!(self.out, "%ss{}", slot.0)?;
            }
            _ => write!(self.out, "%v{inst}")?,
        }
        Ok(())
    }

    fn int_literal(&mut self, ty: Ty, imm: u32) -> Result<(), LowerError> {
        let bits = ty.bits(self.view.head.ptr_bits);
        if bits == 1 {
            self.out.push_str(if imm & 1 != 0 { "true" } else { "false" });
        } else if bits <= 32 {
            write!(self.out, "{imm}")?;
        } else {
            write!(self.out, "{}", imm as i32)?;
        }
        Ok(())
    }

    /// `T v`.
    fn typed(&mut self, v: Value) -> Result<(), LowerError> {
        self.ty(self.view.ty(v))?;
        self.out.push(' ');
        self.val(v)
    }

    fn arg(&mut self, arg: Arg<'_>) -> Result<(), LowerError> {
        match arg {
            Arg::V(v) => self.typed(v),
            Arg::Lit(ty, text) => {
                write!(self.out, "{ty} {text}")?;
                Ok(())
            }
            Arg::Tmp(ty, inst, suffix) => {
                self.ty(ty)?;
                write!(self.out, " %v{inst}.{suffix}")?;
                Ok(())
            }
        }
    }

    fn lhs(&mut self, inst: usize) -> Result<(), LowerError> {
        write!(self.out, "  %v{inst} = ")?;
        Ok(())
    }

    /// `  [%v<inst> = ]call RET @llvm.<base>.<mangles>(args)`, and its declaration.
    fn intrinsic(
        &mut self,
        result: Option<(usize, Ty)>,
        base: &str,
        mangles: &[Ty],
        args: &[Arg<'_>],
    ) -> Result<(), LowerError> {
        let mut name: AString<'a> = String::new_in(self.cx.alloc);
        write!(name, "llvm.{base}")?;
        for &ty in mangles {
            name.push('.');
            self.mangle(&mut name, ty)?;
        }
        // The declaration: built in `self.out`'s place, then moved to the declarations.
        let saved = core::mem::replace(self.out, String::new_in(self.cx.alloc));
        let built = (|| -> Result<(), LowerError> {
            self.out.push_str("declare ");
            match result {
                Some((_, ty)) => self.ty(ty)?,
                None => self.out.push_str("void"),
            }
            write!(self.out, " @{}(", name.as_str())?;
            for (j, arg) in args.iter().enumerate() {
                if j > 0 {
                    self.out.push_str(", ");
                }
                match *arg {
                    Arg::V(v) => self.ty(self.view.ty(v))?,
                    Arg::Lit(ty, _) => self.out.push_str(ty),
                    Arg::Tmp(ty, _, _) => self.ty(ty)?,
                }
            }
            self.out.push(')');
            Ok(())
        })();
        let decl = core::mem::replace(self.out, saved);
        built?;
        self.decls.push(decl);
        match result {
            Some((inst, ty)) => {
                self.lhs(inst)?;
                self.out.push_str("call ");
                self.ty(ty)?;
            }
            None => self.out.push_str("  call void"),
        }
        write!(self.out, " @{}(", name.as_str())?;
        for (j, &arg) in args.iter().enumerate() {
            if j > 0 {
                self.out.push_str(", ");
            }
            self.arg(arg)?;
        }
        self.out.push_str(")\n");
        Ok(())
    }

    fn fmf(&mut self, flags: Flags) -> Result<(), LowerError> {
        if flags.contains(Flags::FAST) {
            self.out.push_str(" fast");
            return Ok(());
        }
        for (flag, name) in [
            (Flags::NNAN, "nnan"),
            (Flags::NINF, "ninf"),
            (Flags::NSZ, "nsz"),
            (Flags::ARCP, "arcp"),
            (Flags::CONTRACT, "contract"),
            (Flags::AFN, "afn"),
            (Flags::REASSOC, "reassoc"),
        ] {
            if flags.contains(flag) {
                write!(self.out, " {name}")?;
            }
        }
        Ok(())
    }

    fn align_of(&self, inst: usize) -> u64 {
        self.view.mem_attrs(inst).map_or(1, |it| 1u64 << it.align.min(32))
    }

    fn ordering(&self, inst: usize, ordering: Option<Ordering>) -> Result<&'static str, LowerError> {
        match ordering {
            Some(Ordering::SeqCst) => Ok("seq_cst"),
            Some(Ordering::Relaxed) => Ok("monotonic"),
            Some(Ordering::Acquire) => Ok("acquire"),
            Some(Ordering::Release) => Ok("release"),
            Some(Ordering::AcqRel) => Ok("acq_rel"),
            None => Err(self.refuse(Some(inst), "an atomic without an ordering")),
        }
    }

    fn scope(&mut self, flags: Flags) {
        if flags.contains(Flags::SINGLE_THREAD) {
            self.out.push_str(" syncscope(\"singlethread\")");
        }
    }

    /// `  br i1 %v<inst>.<cond>, label %v<inst>.<trap>, label %v<inst>.<ok>` and the trap block;
    /// the code goes on in `v<inst>.<ok>`.
    fn trap_if(&mut self, inst: usize, cond: &str, tag: &str) -> Result<(), LowerError> {
        self.decls.push(text_in("declare void @llvm.trap()", self.cx.alloc));
        writeln!(
            self.out,
            "  br i1 %v{inst}.{cond}, label %v{inst}.{tag}t, label %v{inst}.{tag}k\nv{inst}.{tag}t:\n  call void @llvm.trap()\n  unreachable\nv{inst}.{tag}k:"
        )?;
        Ok(())
    }

    /// A shift's or rotate's amount in the shifted value's type: masked to the width unless
    /// `masked` is false (a rotate takes it modulo the width itself) or the instruction
    /// promises it in range. Returns the argument naming it.
    fn amount(&mut self, inst: usize, a: Value, b: Value, masked: bool) -> Result<Arg<'static>, LowerError> {
        let (ta, tb) = (self.view.ty(a), self.view.ty(b));
        let in_range = self.view.flags(inst).contains(Flags::IN_RANGE);
        if ta.is_vector() || tb.is_vector() {
            if ta != tb {
                return Err(self.refuse(Some(inst), "a vector shift by an amount of another type"));
            }
            if !masked || in_range {
                return Ok(Arg::V(b));
            }
            let width = ta.elem().bits(self.view.head.ptr_bits);
            write!(self.out, "  %v{inst}.m = and ")?;
            self.typed(b)?;
            writeln!(self.out, ", splat ({} {})", scalar(ta.elem()), width - 1)?;
            return Ok(Arg::Tmp(tb, inst as u32, "m"));
        }
        let ptr_bits = self.view.head.ptr_bits;
        let (wa, wb) = (ta.bits(ptr_bits), tb.bits(ptr_bits));
        let mut current = Arg::V(b);
        if masked && !in_range {
            write!(self.out, "  %v{inst}.m = and ")?;
            self.typed(b)?;
            writeln!(self.out, ", {}", wa - 1)?;
            current = Arg::Tmp(tb, inst as u32, "m");
        }
        if wa != wb {
            let cast = if wb < wa { "zext" } else { "trunc" };
            write!(self.out, "  %v{inst}.c = {cast} ")?;
            self.arg(current)?;
            self.out.push_str(" to ");
            self.ty(ta)?;
            self.out.push('\n');
            current = Arg::Tmp(ta, inst as u32, "c");
        }
        Ok(current)
    }

    // ---- Instructions.

    fn inst(&mut self, inst: usize) -> Result<(), LowerError> {
        let view = self.view;
        let op = view.op(inst);
        let flags = view.flags(inst);
        let ty = view.ty(Value(inst as u32));
        let ptr_bits = view.head.ptr_bits;
        let mut o = view.operands(inst);
        match op {
            // Inline operands: no instruction.
            Op::Param
            | Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Vconst
            | Op::Null
            | Op::Uninit
            | Op::FuncAddr
            | Op::DataAddr
            | Op::SlotAddr => {}
            // Lifetime markers: promises LLVM is not given (see the module).
            Op::SlotLive | Op::SlotDead => {}

            Op::Add | Op::Sub | Op::Mul => {
                let (a, b) = (o.value(), o.value());
                self.lhs(inst)?;
                self.out.push_str(match op {
                    Op::Add => "add",
                    Op::Sub => "sub",
                    _ => "mul",
                });
                if flags.contains(Flags::NUW) {
                    self.out.push_str(" nuw");
                }
                if flags.contains(Flags::NSW) {
                    self.out.push_str(" nsw");
                }
                self.binary_tail(a, b)?;
            }
            Op::Neg => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str("sub ");
                self.ty(ty)?;
                self.out.push_str(if ty.is_vector() { " zeroinitializer, " } else { " 0, " });
                self.val(a)?;
                self.out.push('\n');
            }
            Op::And | Op::Or | Op::Xor => {
                let (a, b) = (o.value(), o.value());
                self.lhs(inst)?;
                self.out.push_str(match op {
                    Op::And => "and",
                    Op::Or => "or",
                    _ => "xor",
                });
                if op == Op::Or && flags.contains(Flags::DISJOINT) {
                    self.out.push_str(" disjoint");
                }
                self.binary_tail(a, b)?;
            }
            Op::Not => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str("xor ");
                self.typed(a)?;
                if ty.is_vector() {
                    writeln!(self.out, ", splat ({} -1)", scalar(ty.elem()))?;
                } else {
                    self.out.push_str(if ty.bits(ptr_bits) == 1 { ", true\n" } else { ", -1\n" });
                }
            }
            Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => self.division(inst, op, flags, ty)?,
            Op::Shl | Op::Lshr | Op::Ashr => {
                let (a, b) = (o.value(), o.value());
                let amount = self.amount(inst, a, b, true)?;
                self.lhs(inst)?;
                self.out.push_str(match op {
                    Op::Shl => "shl",
                    Op::Lshr => "lshr",
                    _ => "ashr",
                });
                if op == Op::Shl {
                    if flags.contains(Flags::NUW) {
                        self.out.push_str(" nuw");
                    }
                    if flags.contains(Flags::NSW) {
                        self.out.push_str(" nsw");
                    }
                } else if flags.contains(Flags::EXACT) {
                    self.out.push_str(" exact");
                }
                self.out.push(' ');
                self.typed(a)?;
                self.out.push_str(", ");
                match amount {
                    Arg::V(v) => self.val(v)?,
                    Arg::Tmp(_, at, suffix) => write!(self.out, "%v{at}.{suffix}")?,
                    Arg::Lit(_, text) => self.out.push_str(text),
                }
                self.out.push('\n');
            }
            Op::Rotl | Op::Rotr => {
                let (a, b) = (o.value(), o.value());
                let amount = self.amount(inst, a, b, false)?;
                let base = if op == Op::Rotl { "fshl" } else { "fshr" };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a), Arg::V(a), amount])?;
            }
            Op::Umulhi | Op::Smulhi => {
                let (a, b) = (o.value(), o.value());
                if !ty.is_scalar() {
                    return Err(self.refuse(Some(inst), "a vector high multiply"));
                }
                let bits = ty.bits(ptr_bits);
                let ext = if op == Op::Umulhi { "zext" } else { "sext" };
                for (tag, v) in [("x", a), ("y", b)] {
                    write!(self.out, "  %v{inst}.{tag} = {ext} ")?;
                    self.typed(v)?;
                    writeln!(self.out, " to i{}", 2 * bits)?;
                }
                writeln!(
                    self.out,
                    "  %v{inst}.p = mul i{w} %v{inst}.x, %v{inst}.y\n  %v{inst}.h = lshr i{w} %v{inst}.p, {bits}\n  %v{inst} = trunc i{w} %v{inst}.h to i{bits}",
                    w = 2 * bits
                )?;
            }
            Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat | Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let (a, b) = (o.value(), o.value());
                let base = match op {
                    Op::UaddSat => "uadd.sat",
                    Op::SaddSat => "sadd.sat",
                    Op::UsubSat => "usub.sat",
                    Op::SsubSat => "ssub.sat",
                    Op::Umin => "umin",
                    Op::Umax => "umax",
                    Op::Smin => "smin",
                    _ => "smax",
                };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a), Arg::V(b)])?;
            }
            Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow | Op::UmulOverflow | Op::SmulOverflow => {
                let (a, b) = (o.value(), o.value());
                let base = match op {
                    Op::UaddOverflow => "uadd.with.overflow",
                    Op::SaddOverflow => "sadd.with.overflow",
                    Op::UsubOverflow => "usub.with.overflow",
                    Op::SsubOverflow => "ssub.with.overflow",
                    Op::UmulOverflow => "umul.with.overflow",
                    _ => "smul.with.overflow",
                };
                self.intrinsic(Some((inst, ty)), base, &[view.ty(a)], &[Arg::V(a), Arg::V(b)])?;
            }
            Op::Clz | Op::Ctz => {
                let a = o.value();
                let base = if op == Op::Clz { "ctlz" } else { "cttz" };
                let poison = if flags.contains(Flags::NONZERO) { "true" } else { "false" };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a), Arg::Lit("i1", poison)])?;
            }
            Op::Popcnt | Op::Bswap | Op::Bitrev => {
                let a = o.value();
                let base = match op {
                    Op::Popcnt => "ctpop",
                    Op::Bswap => "bswap",
                    _ => "bitreverse",
                };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a)])?;
            }
            Op::Icmp => {
                let cond = o.int_cond();
                let (a, b) = (o.value(), o.value());
                self.lhs(inst)?;
                write!(self.out, "icmp {} ", cond.name())?;
                self.typed(a)?;
                self.out.push_str(", ");
                self.val(b)?;
                self.out.push('\n');
            }
            Op::Fcmp => {
                let cond = o.float_cond();
                let (a, b) = (o.value(), o.value());
                self.lhs(inst)?;
                self.out.push_str("fcmp");
                self.fmf(flags)?;
                write!(self.out, " {} ", cond.name())?;
                self.typed(a)?;
                self.out.push_str(", ");
                self.val(b)?;
                self.out.push('\n');
            }
            Op::Select => {
                let (c, a, b) = (o.value(), o.value(), o.value());
                self.lhs(inst)?;
                self.out.push_str("select ");
                self.typed(c)?;
                self.out.push_str(", ");
                self.typed(a)?;
                self.out.push_str(", ");
                self.typed(b)?;
                if flags.contains(Flags::UNPREDICTABLE) {
                    self.out.push_str(", !unpredictable !0");
                }
                self.out.push('\n');
            }
            Op::Zext | Op::Sext | Op::Trunc | Op::Fpext | Op::Fptrunc | Op::UitoFp | Op::SitoFp | Op::Bitcast => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str(match op {
                    Op::Zext => "zext ",
                    Op::Sext => "sext ",
                    Op::Trunc => "trunc ",
                    Op::Fpext => "fpext ",
                    Op::Fptrunc => "fptrunc ",
                    Op::UitoFp => "uitofp ",
                    Op::SitoFp => "sitofp ",
                    _ => "bitcast ",
                });
                self.typed(a)?;
                self.out.push_str(" to ");
                self.ty(ty)?;
                self.out.push('\n');
            }
            Op::FptoUi | Op::FptoSi => {
                let a = o.value();
                let base = if op == Op::FptoUi { "fptoui.sat" } else { "fptosi.sat" };
                self.intrinsic(Some((inst, ty)), base, &[ty, view.ty(a)], &[Arg::V(a)])?;
            }
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Frem => {
                let (a, b) = (o.value(), o.value());
                self.lhs(inst)?;
                self.out.push_str(match op {
                    Op::Fadd => "fadd",
                    Op::Fsub => "fsub",
                    Op::Fmul => "fmul",
                    Op::Fdiv => "fdiv",
                    _ => "frem",
                });
                self.fmf(flags)?;
                self.binary_tail(a, b)?;
            }
            Op::Fneg => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str("fneg");
                self.fmf(flags)?;
                self.out.push(' ');
                self.typed(a)?;
                self.out.push('\n');
            }
            Op::FminNum | Op::FmaxNum | Op::Fminimum | Op::Fmaximum | Op::Fcopysign => {
                let (a, b) = (o.value(), o.value());
                let base = match op {
                    Op::FminNum => "minnum",
                    Op::FmaxNum => "maxnum",
                    Op::Fminimum => "minimum",
                    Op::Fmaximum => "maximum",
                    _ => "copysign",
                };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a), Arg::V(b)])?;
            }
            Op::Fabs | Op::Sqrt | Op::Floor | Op::Ceil | Op::Ftrunc | Op::Nearest | Op::Fround => {
                let a = o.value();
                let base = match op {
                    Op::Fabs => "fabs",
                    Op::Sqrt => "sqrt",
                    Op::Floor => "floor",
                    Op::Ceil => "ceil",
                    Op::Ftrunc => "trunc",
                    Op::Nearest => "roundeven",
                    _ => "round",
                };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a)])?;
            }
            Op::Fma | Op::Fmuladd => {
                let (a, b, c) = (o.value(), o.value(), o.value());
                let base = if op == Op::Fma { "fma" } else { "fmuladd" };
                self.intrinsic(Some((inst, ty)), base, &[ty], &[Arg::V(a), Arg::V(b), Arg::V(c)])?;
            }
            Op::Freeze => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str("freeze ");
                self.typed(a)?;
                self.out.push('\n');
            }
            Op::Assume => {
                let a = o.value();
                self.intrinsic(None, "assume", &[], &[Arg::V(a)])?;
            }
            Op::PtrAdd => {
                let (a, b) = (o.value(), o.value());
                self.lhs(inst)?;
                self.out.push_str("getelementptr ");
                if flags.contains(Flags::INBOUNDS) {
                    self.out.push_str("inbounds ");
                }
                self.out.push_str("i8, ");
                self.typed(a)?;
                self.out.push_str(", ");
                self.typed(b)?;
                self.out.push('\n');
            }
            Op::PtrAddr | Op::Expose => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str("ptrtoint ");
                self.typed(a)?;
                self.out.push_str(" to ");
                self.ty(ty)?;
                self.out.push('\n');
            }
            Op::FromExposed => {
                let a = o.value();
                self.lhs(inst)?;
                self.out.push_str("inttoptr ");
                self.typed(a)?;
                self.out.push_str(" to ptr\n");
            }
            Op::PtrWithAddr => {
                // The address `b` with `a`'s provenance: `a` moved by the difference.
                let (a, b) = (o.value(), o.value());
                let tb = view.ty(b);
                write!(self.out, "  %v{inst}.t = ptrtoint ")?;
                self.typed(a)?;
                self.out.push_str(" to ");
                self.ty(tb)?;
                write!(self.out, "\n  %v{inst}.d = sub ")?;
                self.typed(b)?;
                write!(self.out, ", %v{inst}.t\n  %v{inst} = getelementptr i8, ")?;
                self.typed(a)?;
                self.out.push_str(", ");
                self.ty(tb)?;
                writeln!(self.out, " %v{inst}.d")?;
            }
            Op::Load => {
                let (p, offset) = (o.value(), o.offset());
                let addr = self.address(inst, p, offset, flags)?;
                self.lhs(inst)?;
                self.out.push_str("load ");
                if flags.contains(Flags::VOLATILE) {
                    self.out.push_str("volatile ");
                }
                self.ty(ty)?;
                self.out.push_str(", ");
                self.arg(addr)?;
                write!(self.out, ", align {}", self.align_of(inst))?;
                if let Some(attrs) = view.mem_attrs(inst) {
                    if attrs.bits & MemAttrsRec::NONNULL != 0 && ty.is_ptr() {
                        self.out.push_str(", !nonnull !0");
                    }
                    if attrs.bits & MemAttrsRec::NOUNDEF != 0 {
                        self.out.push_str(", !noundef !0");
                    }
                    if attrs.bits & MemAttrsRec::INVARIANT != 0 {
                        self.out.push_str(", !invariant.load !0");
                    }
                }
                self.out.push('\n');
            }
            Op::Store => {
                let (v, p, offset) = (o.value(), o.value(), o.offset());
                let addr = self.address(inst, p, offset, flags)?;
                self.out.push_str("  store ");
                if flags.contains(Flags::VOLATILE) {
                    self.out.push_str("volatile ");
                }
                self.typed(v)?;
                self.out.push_str(", ");
                self.arg(addr)?;
                writeln!(self.out, ", align {}", self.align_of(inst))?;
            }
            Op::Memcpy | Op::Memmove | Op::Memset => {
                let (d, s, n) = (o.value(), o.value(), o.value());
                let tn = view.ty(n);
                let volatile = if flags.contains(Flags::VOLATILE) { "true" } else { "false" };
                let mut name: AString<'a> = String::new_in(self.cx.alloc);
                name.push_str(match op {
                    Op::Memcpy => "llvm.memcpy.p0.p0.",
                    Op::Memmove => "llvm.memmove.p0.p0.",
                    _ => "llvm.memset.p0.",
                });
                self.mangle(&mut name, tn)?;
                let mut decl: AString<'a> = String::new_in(self.cx.alloc);
                let second = if op == Op::Memset { "i8" } else { "ptr" };
                write!(decl, "declare void @{}(ptr, {second}, ", name.as_str())?;
                let saved = core::mem::replace(self.out, decl);
                let typed = self.ty(tn);
                let mut decl = core::mem::replace(self.out, saved);
                typed?;
                decl.push_str(", i1)");
                self.decls.push(decl);
                if op == Op::Memset && view.ty(s) != Ty::I8 {
                    return Err(self.refuse(Some(inst), "a memset whose byte is not an i8"));
                }
                let attrs = view.mem_attrs(inst);
                write!(self.out, "  call void @{}(ptr align {} ", name.as_str(), self.align_of(inst))?;
                self.val(d)?;
                if op == Op::Memset {
                    self.out.push_str(", ");
                    self.typed(s)?;
                } else {
                    write!(self.out, ", ptr align {} ", attrs.map_or(1, |it| 1u64 << it.src_align.min(32)))?;
                    self.val(s)?;
                }
                self.out.push_str(", ");
                self.typed(n)?;
                writeln!(self.out, ", i1 {volatile})")?;
            }
            Op::Memcmp => {
                let (a, b, n) = (o.value(), o.value(), o.value());
                let mut decl: AString<'a> = String::new_in(self.cx.alloc);
                decl.push_str("declare i32 @memcmp(ptr, ptr, ");
                let saved = core::mem::replace(self.out, decl);
                let typed = self.ty(view.ty(n));
                let mut decl = core::mem::replace(self.out, saved);
                typed?;
                decl.push(')');
                self.decls.push(decl);
                self.lhs(inst)?;
                self.out.push_str("call i32 @memcmp(");
                self.typed(a)?;
                self.out.push_str(", ");
                self.typed(b)?;
                self.out.push_str(", ");
                self.typed(n)?;
                self.out.push_str(")\n");
            }
            Op::AtomicLoad => {
                let p = o.value();
                let order = self.ordering(inst, flags.ordering())?;
                self.lhs(inst)?;
                self.out.push_str("load atomic ");
                if flags.contains(Flags::VOLATILE) {
                    self.out.push_str("volatile ");
                }
                self.ty(ty)?;
                self.out.push_str(", ");
                self.typed(p)?;
                self.scope(flags);
                writeln!(self.out, " {order}, align {}", self.atomic_align(inst, ty))?;
            }
            Op::AtomicStore => {
                let (v, p) = (o.value(), o.value());
                let order = self.ordering(inst, flags.ordering())?;
                self.out.push_str("  store atomic ");
                if flags.contains(Flags::VOLATILE) {
                    self.out.push_str("volatile ");
                }
                self.typed(v)?;
                self.out.push_str(", ");
                self.typed(p)?;
                self.scope(flags);
                writeln!(self.out, " {order}, align {}", self.atomic_align(inst, view.ty(v)))?;
            }
            Op::AtomicRmw => {
                let rmw = o.rmw();
                let (p, v) = (o.value(), o.value());
                let order = self.ordering(inst, flags.ordering())?;
                self.lhs(inst)?;
                self.out.push_str("atomicrmw ");
                if flags.contains(Flags::VOLATILE) {
                    self.out.push_str("volatile ");
                }
                write!(self.out, "{} ", rmw.name())?;
                self.typed(p)?;
                self.out.push_str(", ");
                self.typed(v)?;
                self.scope(flags);
                writeln!(self.out, " {order}, align {}", self.atomic_align(inst, ty))?;
            }
            Op::Cmpxchg => {
                let (p, e, n) = (o.value(), o.value(), o.value());
                let success = self.ordering(inst, flags.ordering())?;
                let failure = self.ordering(inst, flags.failure())?;
                self.lhs(inst)?;
                self.out.push_str("cmpxchg ");
                if flags.contains(Flags::WEAK) {
                    self.out.push_str("weak ");
                }
                if flags.contains(Flags::VOLATILE) {
                    self.out.push_str("volatile ");
                }
                self.typed(p)?;
                self.out.push_str(", ");
                self.typed(e)?;
                self.out.push_str(", ");
                self.typed(n)?;
                self.scope(flags);
                writeln!(self.out, " {success} {failure}, align {}", self.atomic_align(inst, view.ty(e)))?;
            }
            Op::Fence => {
                let order = self.ordering(inst, flags.ordering())?;
                self.out.push_str("  fence");
                self.scope(flags);
                writeln!(self.out, " {order}")?;
            }
            Op::Splat => {
                let a = o.value();
                write!(self.out, "  %v{inst}.i = insertelement ")?;
                self.ty(ty)?;
                self.out.push_str(" poison, ");
                self.typed(a)?;
                write!(self.out, ", i64 0\n  %v{inst} = shufflevector ")?;
                self.ty(ty)?;
                write!(self.out, " %v{inst}.i, ")?;
                self.ty(ty)?;
                writeln!(self.out, " poison, <{} x i32> zeroinitializer", ty.lanes())?;
            }
            Op::Extract => {
                let (a, lane) = (o.value(), o.imm());
                self.lhs(inst)?;
                self.out.push_str("extractelement ");
                self.typed(a)?;
                writeln!(self.out, ", i64 {lane}")?;
            }
            Op::Insert => {
                let (a, b, lane) = (o.value(), o.value(), o.imm());
                self.lhs(inst)?;
                self.out.push_str("insertelement ");
                self.typed(a)?;
                self.out.push_str(", ");
                self.typed(b)?;
                writeln!(self.out, ", i64 {lane}")?;
            }
            Op::Shuffle => {
                let (a, b) = (o.value(), o.value());
                let mask = o.mask();
                self.lhs(inst)?;
                self.out.push_str("shufflevector ");
                self.typed(a)?;
                self.out.push_str(", ");
                self.typed(b)?;
                write!(self.out, ", <{} x i32> <", mask.len())?;
                for (j, lane) in mask.iter().enumerate() {
                    write!(self.out, "{}i32 {lane}", if j == 0 { "" } else { ", " })?;
                }
                self.out.push_str(">\n");
            }
            Op::ReduceAdd
            | Op::ReduceMul
            | Op::ReduceAnd
            | Op::ReduceOr
            | Op::ReduceXor
            | Op::ReduceUmin
            | Op::ReduceUmax
            | Op::ReduceSmin
            | Op::ReduceSmax
            | Op::ReduceFmin
            | Op::ReduceFmax => {
                let a = o.value();
                let base = match op {
                    Op::ReduceAdd => "vector.reduce.add",
                    Op::ReduceMul => "vector.reduce.mul",
                    Op::ReduceAnd => "vector.reduce.and",
                    Op::ReduceOr => "vector.reduce.or",
                    Op::ReduceXor => "vector.reduce.xor",
                    Op::ReduceUmin => "vector.reduce.umin",
                    Op::ReduceUmax => "vector.reduce.umax",
                    Op::ReduceSmin => "vector.reduce.smin",
                    Op::ReduceSmax => "vector.reduce.smax",
                    Op::ReduceFmin => "vector.reduce.fmin",
                    _ => "vector.reduce.fmax",
                };
                self.intrinsic(Some((inst, ty)), base, &[view.ty(a)], &[Arg::V(a)])?;
            }
            Op::Call | Op::CallIndirect | Op::TailCall | Op::TailCallIndirect | Op::Invoke | Op::InvokeIndirect => {
                self.call(inst, op)?
            }
            Op::Resume => self.resume(inst)?,
            Op::Project => {
                let (a, member) = (o.value(), o.imm());
                self.lhs(inst)?;
                self.out.push_str("extractvalue ");
                self.typed(a)?;
                writeln!(self.out, ", {member}")?;
            }
            Op::FrameAddress => {
                self.intrinsic(Some((inst, Ty::PTR)), "frameaddress", &[Ty::PTR], &[Arg::Lit("i32", "0")])?;
            }
            Op::ReturnAddress => {
                self.intrinsic(Some((inst, Ty::PTR)), "returnaddress", &[], &[Arg::Lit("i32", "0")])?;
            }
            Op::Jump => {
                let target = o.target();
                self.out.push_str("  br ");
                self.label(inst, 0, &target)?;
                self.out.push('\n');
            }
            Op::Br => {
                let (c, yes, no) = (o.value(), o.target(), o.target());
                self.out.push_str("  br ");
                self.typed(c)?;
                self.out.push_str(", ");
                self.label(inst, 0, &yes)?;
                self.out.push_str(", ");
                self.label(inst, 1, &no)?;
                if flags.contains(Flags::EXPECT_TRUE) {
                    self.out.push_str(", !prof !1");
                } else if flags.contains(Flags::EXPECT_FALSE) {
                    self.out.push_str(", !prof !2");
                }
                if flags.contains(Flags::UNPREDICTABLE) {
                    self.out.push_str(", !unpredictable !0");
                }
                self.out.push('\n');
            }
            Op::Switch => {
                let scrutinee = o.value();
                let default = o.target();
                let ts = view.ty(scrutinee);
                self.out.push_str("  switch ");
                self.typed(scrutinee)?;
                self.out.push_str(", ");
                self.label(inst, 0, &default)?;
                self.out.push_str(" [");
                for (k, (words, target)) in o.cases(scrutinee).enumerate() {
                    self.out.push(' ');
                    self.ty(ts)?;
                    write!(self.out, " {}, ", words_value(words))?;
                    self.label(inst, k + 1, &target)?;
                }
                self.out.push_str(" ]\n");
            }
            Op::Return => {
                let values = o.values();
                let sig = view.sig(SigId(view.head.sig));
                match values.len() {
                    0 => self.out.push_str("  ret void\n"),
                    1 => {
                        self.out.push_str("  ret ");
                        self.typed(values.get(0))?;
                        self.out.push('\n');
                    }
                    n => {
                        for j in 0..n {
                            write!(self.out, "  %v{inst}.r{j} = insertvalue ")?;
                            self.ret_text(sig)?;
                            if j == 0 {
                                self.out.push_str(" poison, ");
                            } else {
                                write!(self.out, " %v{inst}.r{}, ", j - 1)?;
                            }
                            self.typed(values.get(j))?;
                            writeln!(self.out, ", {j}")?;
                        }
                        self.out.push_str("  ret ");
                        self.ret_text(sig)?;
                        writeln!(self.out, " %v{inst}.r{}", n - 1)?;
                    }
                }
            }
            Op::Unreachable => self.out.push_str("  unreachable\n"),
            Op::Trap => {
                let code = o.imm();
                if code > 255 {
                    return Err(self.refuse(Some(inst), "a trap code past 255"));
                }
                self.decls.push(text_in("declare void @llvm.ubsantrap(i8 immarg)", self.cx.alloc));
                writeln!(self.out, "  call void @llvm.ubsantrap(i8 {code})\n  unreachable")?;
            }
            Op::Swizzle => return Err(self.refuse(Some(inst), "`swizzle`")),
            Op::Bitmask => return Err(self.refuse(Some(inst), "`bitmask`")),
            Op::ReduceFadd | Op::ReduceFmul => return Err(self.refuse(Some(inst), "a float reduction with a start value")),
            Op::MaskedLoad | Op::MaskedStore | Op::Gather | Op::Scatter => {
                return Err(self.refuse(Some(inst), "masked or gathered memory"));
            }
            Op::TlsAddr => return Err(self.refuse(Some(inst), "a thread-local address")),
            Op::StackPointer => return Err(self.refuse(Some(inst), "`stack_pointer`")),
            Op::TargetIntrinsic => return Err(self.refuse(Some(inst), "a target intrinsic")),
            Op::Asm | Op::AsmBr => return Err(self.refuse(Some(inst), "`asm!`")),
            Op::Suspend | Op::Cancelled => return Err(self.refuse(Some(inst), "a coroutine's suspension")),
        }
        Ok(())
    }

    /// ` T a, b\n` after a binary operation's name and flags.
    fn binary_tail(&mut self, a: Value, b: Value) -> Result<(), LowerError> {
        self.out.push(' ');
        self.typed(a)?;
        self.out.push_str(", ");
        self.val(b)?;
        self.out.push('\n');
        Ok(())
    }

    /// LIR division: a zero divisor traps, `MIN / -1` traps, `MIN % -1` is 0; LLVM is told
    /// none of it may happen only where the instruction promises so (`nonzero`, `nsw`).
    fn division(&mut self, inst: usize, op: Op, flags: Flags, ty: Ty) -> Result<(), LowerError> {
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        let signed = matches!(op, Op::Sdiv | Op::Srem);
        let checked_zero = !flags.contains(Flags::NONZERO);
        let checked_overflow = signed && !flags.contains(Flags::NSW);
        if ty.is_vector() && (checked_zero || checked_overflow) {
            return Err(self.refuse(Some(inst), "a vector division without the `nonzero` and `nsw` promises"));
        }
        let bits = ty.bits(self.view.head.ptr_bits);
        if checked_zero {
            write!(self.out, "  %v{inst}.z = icmp eq ")?;
            self.typed(b)?;
            self.out.push_str(", 0\n");
            self.trap_if(inst, "z", "z")?;
        }
        let mut divisor = Arg::V(b);
        if checked_overflow {
            let min = 1u128 << (bits - 1);
            write!(self.out, "  %v{inst}.m = icmp eq ")?;
            self.typed(a)?;
            write!(self.out, ", {min}\n  %v{inst}.n = icmp eq ")?;
            self.typed(b)?;
            writeln!(self.out, ", -1\n  %v{inst}.o = and i1 %v{inst}.m, %v{inst}.n")?;
            if op == Op::Sdiv {
                self.trap_if(inst, "o", "o")?;
            } else {
                // `MIN % -1` is 0: divide by 1 there instead.
                write!(self.out, "  %v{inst}.b = select i1 %v{inst}.o, ")?;
                self.ty(ty)?;
                self.out.push_str(" 1, ");
                self.typed(b)?;
                self.out.push('\n');
                divisor = Arg::Tmp(ty, inst as u32, "b");
            }
        }
        self.lhs(inst)?;
        self.out.push_str(match op {
            Op::Udiv => "udiv",
            Op::Sdiv => "sdiv",
            Op::Urem => "urem",
            _ => "srem",
        });
        if matches!(op, Op::Udiv | Op::Sdiv) && flags.contains(Flags::EXACT) {
            self.out.push_str(" exact");
        }
        self.out.push(' ');
        self.typed(a)?;
        self.out.push_str(", ");
        match divisor {
            Arg::V(v) => self.val(v)?,
            Arg::Tmp(_, at, suffix) => write!(self.out, "%v{at}.{suffix}")?,
            Arg::Lit(_, text) => self.out.push_str(text),
        }
        self.out.push('\n');
        Ok(())
    }

    /// A load's or store's address: `p`, or `p` moved by `offset` bytes.
    fn address(&mut self, inst: usize, p: Value, offset: i32, flags: Flags) -> Result<Arg<'static>, LowerError> {
        if offset == 0 {
            return Ok(Arg::V(p));
        }
        write!(self.out, "  %v{inst}.a = getelementptr ")?;
        if flags.contains(Flags::INBOUNDS) {
            self.out.push_str("inbounds ");
        }
        self.out.push_str("i8, ");
        self.typed(p)?;
        writeln!(self.out, ", i{} {offset}", self.view.head.ptr_bits)?;
        Ok(Arg::Tmp(Ty::PTR, inst as u32, "a"))
    }

    fn atomic_align(&self, inst: usize, ty: Ty) -> u64 {
        match self.view.mem_attrs(inst) {
            Some(attrs) => 1u64 << attrs.align.min(32),
            None => u64::from(ty.bits(self.view.head.ptr_bits) / 8).max(1),
        }
    }

    fn call(&mut self, inst: usize, op: Op) -> Result<(), LowerError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let direct = matches!(op, Op::Call | Op::TailCall | Op::Invoke);
        let (callee, pointer) = if direct { (Some(o.reference()), None) } else { (None, Some(o.value())) };
        let sig = view.sig(o.sig());
        // An invoke's return and unwind blocks, in that order, before its arguments.
        let invoke = matches!(op, Op::Invoke | Op::InvokeIndirect).then(|| (o.block(), o.block()));
        let args = o.values();
        let tail = matches!(op, Op::TailCall | Op::TailCallIndirect);
        let has_result = !sig.rets.is_empty();
        if has_result {
            if tail {
                write!(self.out, "  %v{inst}.t = ")?;
            } else {
                self.lhs(inst)?;
            }
        } else {
            self.out.push_str("  ");
        }
        self.out.push_str(match (tail, invoke.is_some()) {
            (true, _) => "musttail call ",
            (false, true) => "invoke ",
            (false, false) => "call ",
        });
        self.conv(sig.conv, Some(inst))?;
        self.ret_text(sig)?;
        if sig.bits & SigRec::VARIADIC != 0 {
            self.out.push_str(" (");
            for (j, param) in sig.params.iter().enumerate() {
                if j > 0 {
                    self.out.push_str(", ");
                }
                self.ty(Ty(param.ty))?;
            }
            self.out.push_str(if sig.params.is_empty() { "...)" } else { ", ...)" });
        }
        self.out.push(' ');
        match (callee, pointer) {
            (Some(r), _) => {
                let rec = view.reference(r);
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    return Err(self.refuse(Some(inst), "a direct call of a reference that is no function"));
                }
                self.ref_name(r, Some(inst))?;
            }
            (None, Some(p)) => self.val(p)?,
            (None, None) => return Err(self.refuse(Some(inst), "a call with no callee")),
        }
        self.out.push('(');
        for (j, arg) in args.iter().enumerate() {
            if j > 0 {
                self.out.push_str(", ");
            }
            self.ty(view.ty(arg))?;
            if let Some(param) = sig.params.get(j) {
                self.param_attrs(param)?;
            }
            self.out.push(' ');
            self.val(arg)?;
        }
        self.out.push(')');
        if let Some((normal, unwind)) = invoke {
            self.invoke_edges(inst, normal, unwind)?;
        }
        self.out.push('\n');
        if tail {
            if has_result {
                self.out.push_str("  ret ");
                self.ret_text(sig)?;
                writeln!(self.out, " %v{inst}.t")?;
            } else {
                self.out.push_str("  ret void\n");
            }
        }
        Ok(())
    }
}

fn scalar(elem: Scalar) -> &'static str {
    match elem {
        Scalar::F16 => "half",
        Scalar::F32 => "float",
        Scalar::F64 => "double",
        Scalar::F128 => "fp128",
        other => other.name(),
    }
}

/// A constant's little-endian words as one unsigned value.
fn words_value(words: &[u32]) -> u128 {
    words.iter().take(4).enumerate().fold(0u128, |acc, (k, &word)| acc | (u128::from(word) << (32 * k)))
}

fn text_in<'a>(text: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    out.push_str(text);
    out
}

/// One `[n x i8] c"..."` piece of a data constant: its type into `pieces`, its value into
/// `values`.
fn piece_bytes(pieces: &mut AString<'_>, values: &mut AString<'_>, bytes: &[u8], first: &mut bool) -> Result<(), LowerError> {
    if !*first {
        pieces.push_str(", ");
        values.push_str(", ");
    }
    *first = false;
    write!(pieces, "[{} x i8]", bytes.len())?;
    write!(values, "[{} x i8] c\"", bytes.len())?;
    for &byte in bytes {
        if (0x20..0x7f).contains(&byte) && byte != b'"' && byte != b'\\' {
            values.push(byte as char);
        } else {
            write!(values, "\\{byte:02X}")?;
        }
    }
    values.push('"');
    Ok(())
}
