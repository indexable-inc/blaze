//! LIR's text form: what `blaze explain` prints of a function, what tests write LIR in, and
//! what [`crate::parse`] reads back to the same columns, byte for byte.
//!
//! A function is a header line, its tables a row a line, then its blocks:
//!
//! ```text
//! func ptr64 sig s0 attrs [nounwind] features ""
//! type t0 = {i64, i1}
//! sig s0 = c (i64, i64) -> (i64)
//! mem m0 = align 3 src_align 0 pointee_align 0 range none noundef
//! block0 normal:
//!   v0 = param i64
//!   v1 = param i64
//!   v2 = uadd_overflow t0 v0, v1
//!   v3 = project i64 v2, 0
//!   return (v3)
//! ```
//!
//! An instruction is `vN = op.flags type operands !mK @spJ` with its value, flags, memory
//! attributes and location where it has them; `vN` is its index, so the numbering is the
//! encoding's. Its operands are its fields in order, comma-separated: `vN`, `blockN(args)`,
//! `(values)`, `[blocks]` or `[lanes]`, an immediate in decimal, a constant as hex words
//! from the most significant, and rows of the tables as `refN`, `sN`, `ssN`, `iN`, `aN`.

use std::fmt::{self, Write as _};

use crate::{
    flags::{AtomicRmw, FloatCond, IntCond, Shown},
    func::{
        AsmOperandKind, AsmPieceRec, Block, Conv, Ext, FnAttrs, Inline, MemAttrsRec, NONE,
        ParamFactsRec, ParamKind, ParamRec, RefKind, SlotKind, View,
    },
    op::Field,
    ty::Ty,
};

/// The text form of `view`.
pub fn print(view: View<'_>) -> String {
    // alloc: the text, asked for by name (`blaze explain`, a test's expectation)
    let mut out = String::new();
    write(view, &mut out).expect("a String takes every write");
    out
}

/// [`print`] into `out`.
pub fn write(view: View<'_>, out: &mut impl fmt::Write) -> fmt::Result {
    header(&view, out)?;
    tables(&view, out)?;
    for b in 0..view.blocks() {
        let block = Block(b as u32);
        let kind = view.block_kind(block);
        writeln!(out, "{block} {}:", kind.name())?;
        for at in view.block(block) {
            out.write_str("  ")?;
            instruction(&view, at, out)?;
            out.write_char('\n')?;
        }
    }
    Ok(())
}

/// `text` as a quoted string: `\\`, `\"` and control characters escaped.
pub(crate) fn quoted(text: &str, out: &mut impl fmt::Write) -> fmt::Result {
    out.write_char('"')?;
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            c if (c as u32) < 0x20 || c as u32 == 0x7f => write!(out, "\\x{:02x}", c as u32)?,
            c => out.write_char(c)?,
        }
    }
    out.write_char('"')
}

fn text<'a>(view: &View<'a>, first: u32, len: u32) -> &'a str {
    std::str::from_utf8(view.asm_str(first, len)).unwrap_or("?")
}

fn hex(bytes: &[u8], out: &mut impl fmt::Write) -> fmt::Result {
    bytes.iter().try_for_each(|byte| write!(out, "{byte:02x}"))
}

fn none_or(word: u32, out: &mut impl fmt::Write) -> fmt::Result {
    match word {
        NONE => out.write_str("none"),
        word => write!(out, "{word}"),
    }
}

/// A 0-or-1-plus-row field as `none` or `{prefix}{row}`.
fn row_or_none(field: u32, prefix: &str, out: &mut impl fmt::Write) -> fmt::Result {
    match field.checked_sub(1) {
        None => out.write_str("none"),
        Some(row) => write!(out, "{prefix}{row}"),
    }
}

fn header(view: &View<'_>, out: &mut impl fmt::Write) -> fmt::Result {
    write!(out, "func ptr{} sig s{} attrs [", view.head.ptr_bits, view.head.sig)?;
    let attrs = FnAttrs(view.head.attrs);
    let names = FnAttrs::NAMED.iter().filter(|it| attrs.0 & it.0 != 0).map(|it| it.1);
    let inline = attrs.inline();
    let mut first = true;
    for name in names {
        if !first {
            out.write_char(' ')?;
        }
        out.write_str(name)?;
        first = false;
    }
    if inline != Inline::None {
        if !first {
            out.write_char(' ')?;
        }
        out.write_str(inline.name())?;
    }
    out.write_str("] features ")?;
    quoted(std::str::from_utf8(view.features).unwrap_or("?"), out)?;
    out.write_char('\n')
}

fn tys(tys: impl Iterator<Item = u16>, out: &mut impl fmt::Write) -> fmt::Result {
    for (i, ty) in tys.enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{}", Ty(ty))?;
    }
    Ok(())
}

fn param(param: &ParamRec, out: &mut impl fmt::Write) -> fmt::Result {
    write!(out, "{}", Ty(param.ty))?;
    for name in [
        Ext::from_byte(param.ext).map_or("?", Ext::name),
        ParamKind::from_byte(param.kind).map_or("?", ParamKind::name),
    ] {
        if !name.is_empty() {
            write!(out, " {name}")?;
        }
    }
    if let Some(row) = param.facts.checked_sub(1) {
        write!(out, " f{row}")?;
    }
    Ok(())
}

fn bits_named(bits: u8, named: &[(u8, &str)], out: &mut impl fmt::Write) -> fmt::Result {
    for &(bit, name) in named {
        if bits & bit != 0 {
            write!(out, " {name}")?;
        }
    }
    Ok(())
}

fn span_ref(span: &blaze_format::SpanRef, out: &mut impl fmt::Write) -> fmt::Result {
    write!(out, "{:x} {}", span.anchor, span.index)
}

fn tables(view: &View<'_>, out: &mut impl fmt::Write) -> fmt::Result {
    for (r, rec) in view.compounds.iter().enumerate() {
        write!(out, "type t{r} = {{")?;
        tys(view.compound_tys[rec.first as usize..][..rec.len as usize].iter().copied(), out)?;
        out.write_str("}\n")?;
    }
    for (r, rec) in view.ranges.iter().enumerate() {
        writeln!(out, "range r{r} = {}, {}", rec.start, rec.end)?;
    }
    for (r, rec) in view.param_facts.iter().enumerate() {
        write!(out, "facts f{r} = deref {} size {} align {} range ", rec.deref, rec.size, rec.align)?;
        row_or_none(rec.range, "r", out)?;
        bits_named(rec.bits, &ParamFactsRec::NAMED, out)?;
        out.write_char('\n')?;
    }
    for (r, rec) in view.sigs.iter().enumerate() {
        let sig = view.sig(crate::func::SigId(r as u32));
        write!(out, "sig s{r} = {} (", Conv::from_byte(rec.conv).map_or("?", Conv::name))?;
        for (i, p) in sig.params.iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            param(p, out)?;
        }
        out.write_str(") -> (")?;
        for (i, p) in sig.rets.iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            param(p, out)?;
        }
        out.write_char(')')?;
        if rec.bits & crate::func::SigRec::VARIADIC != 0 {
            out.write_str(" variadic")?;
        }
        out.write_char('\n')?;
    }
    for (r, rec) in view.mem_attrs.iter().enumerate() {
        write!(
            out,
            "mem m{r} = align {} src_align {} pointee_align {} range ",
            rec.align, rec.src_align, rec.pointee_align
        )?;
        row_or_none(rec.range, "r", out)?;
        bits_named(rec.bits, &MemAttrsRec::NAMED, out)?;
        out.write_char('\n')?;
    }
    for (r, rec) in view.refs.iter().enumerate() {
        match RefKind::from_word(rec.kind) {
            Some(RefKind::Data) => writeln!(out, "ref ref{r} = data d{}", rec.place)?,
            kind => writeln!(out, "ref ref{r} = {} {}", kind.map_or("?", RefKind::name), rec.place)?,
        }
    }
    for (r, rec) in view.datas.iter().enumerate() {
        write!(out, "data d{r} = align {} bytes \"", rec.align)?;
        hex(view.data_bytes_of(crate::func::DataId(r as u32)), out)?;
        out.write_str("\" relocs [")?;
        for (i, reloc) in view.data_relocs_of(crate::func::DataId(r as u32)).iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            write!(out, "{} ref{} {}", reloc.offset, reloc.target, reloc.addend)?;
        }
        out.write_str("]\n")?;
    }
    for (r, rec) in view.intrinsics.iter().enumerate() {
        write!(out, "intrinsic i{r} = ")?;
        quoted(std::str::from_utf8(view.intrinsic_name(crate::func::IntrinsicId(r as u32))).unwrap_or("?"), out)?;
        writeln!(out, " s{}", rec.sig)?;
    }
    for (r, rec) in view.slots.iter().enumerate() {
        let kind = SlotKind::from_byte(rec.kind).map_or("?", SlotKind::name);
        writeln!(out, "slot ss{r} = {} align {} {kind}", rec.size, rec.align)?;
    }
    for (r, rec) in view.asms.iter().enumerate() {
        write!(out, "asm a{r} = options [")?;
        let mut first = true;
        for &(bit, name) in &crate::func::AsmRec::NAMED {
            if rec.options & bit != 0 {
                if !first {
                    out.write_char(' ')?;
                }
                out.write_str(name)?;
                first = false;
            }
        }
        out.write_str("] clobbers ")?;
        quoted(text(view, rec.clobbers_first, rec.clobbers_len), out)?;
        out.write_str(" pieces [")?;
        for (i, piece) in view.asm_pieces_of(crate::func::AsmId(r as u32)).iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            match piece.kind {
                AsmPieceRec::TEXT => quoted(text(view, piece.a, piece.b), out)?,
                _ => match char::from_u32(piece.b).filter(|_| piece.b != 0) {
                    None => write!(out, "{{{}}}", piece.a)?,
                    Some(modifier) => write!(out, "{{{}:{}}}", piece.a, modifier)?,
                },
            }
        }
        out.write_str("] operands [")?;
        for (i, operand) in view.asm_operands_of(crate::func::AsmId(r as u32)).iter().enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            let kind = AsmOperandKind::from_byte(operand.kind).map_or("?", AsmOperandKind::name);
            let class = if operand.class == 0 { "reg" } else { "class" };
            write!(out, "{kind} {class} ")?;
            quoted(text(view, operand.reg_first, operand.reg_len), out)?;
            write!(out, " {}", operand.aux)?;
        }
        out.write_str("]\n")?;
    }
    for (r, span) in view.spans.iter().enumerate() {
        write!(out, "span sp{r} = ")?;
        span_ref(span, out)?;
        if let Some(&scope) = view.span_scopes.get(r) {
            write!(out, " scope {scope}")?;
        }
        if let Some(&inlined) = view.span_inlined.get(r) {
            out.write_str(" inlined ")?;
            none_or(inlined, out)?;
        }
        out.write_char('\n')?;
    }
    for (r, var) in view.variables.iter().enumerate() {
        write!(out, "var x{r} = binding {} slot ss{} pointer ", var.binding, var.slot)?;
        none_or(var.pointer, out)?;
        out.write_str(" scope ")?;
        none_or(var.scope, out)?;
        writeln!(out, " param {} ty {:x}", var.param, var.ty)?;
    }
    for (r, &parent) in view.scope_parents.iter().enumerate() {
        write!(out, "scope sc{r} = parent ")?;
        none_or(parent, out)?;
        out.write_char('\n')?;
    }
    for (r, rec) in view.inlined.iter().enumerate() {
        write!(out, "inline in{r} = parent ")?;
        none_or(rec.parent, out)?;
        out.write_str(" site ")?;
        span_ref(&rec.call_site, out)?;
        out.write_char('\n')?;
    }
    Ok(())
}

/// Words as hex, the most significant first, eight digits each.
fn const_hex(words: &[u32], out: &mut impl fmt::Write) -> fmt::Result {
    out.write_str("0x")?;
    words.iter().rev().try_for_each(|word| write!(out, "{word:08x}"))
}

fn values(values: crate::op::Values<'_>, out: &mut impl fmt::Write) -> fmt::Result {
    out.write_char('(')?;
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.write_str(", ")?;
        }
        write!(out, "{value}")?;
    }
    out.write_char(')')
}

fn target(target: crate::op::Target<'_>, out: &mut impl fmt::Write) -> fmt::Result {
    write!(out, "{}", target.block)?;
    values(target.args, out)
}

/// One instruction's line, without its indent.
pub fn instruction(view: &View<'_>, at: usize, out: &mut impl fmt::Write) -> fmt::Result {
    let op = view.op(at);
    let info = op.info();
    let ty = Ty(view.tys[at]);
    if !ty.is_none() {
        write!(out, "v{at} = ")?;
    }
    write!(out, "{}{}", op.name(), Shown { class: info.flags, flags: view.flags(at) })?;
    if !ty.is_none() {
        write!(out, " {ty}")?;
    }
    let mut o = view.operands(at);
    let mut scrutinee = None;
    for (i, &field) in info.layout.fields().iter().enumerate() {
        out.write_str(if i == 0 { " " } else { ", " })?;
        match field {
            Field::Value => {
                let value = o.value();
                scrutinee = Some(value);
                write!(out, "{value}")?;
            }
            Field::Imm => write!(out, "{}", o.imm())?,
            Field::Offset => write!(out, "{}", o.offset())?,
            Field::IntCond => {
                out.write_str(IntCond::from_word(o.word()).map_or("?", IntCond::name))?
            }
            Field::FloatCond => {
                out.write_str(FloatCond::from_word(o.word()).map_or("?", FloatCond::name))?
            }
            Field::Rmw => out.write_str(AtomicRmw::from_word(o.word()).map_or("?", AtomicRmw::name))?,
            Field::Ref => write!(out, "{}", o.reference())?,
            Field::Sig => write!(out, "{}", o.sig())?,
            Field::Slot => write!(out, "{}", o.slot())?,
            Field::Intrinsic => write!(out, "{}", o.intrinsic())?,
            Field::Asm => write!(out, "{}", o.asm())?,
            Field::Block => write!(out, "{}", o.block())?,
            Field::Target => target(o.target(), out)?,
            Field::Values => values(o.values(), out)?,
            Field::Blocks => {
                out.write_char('[')?;
                for (i, &block) in o.blocks().iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    write!(out, "{}", Block(block))?;
                }
                out.write_char(']')?;
            }
            Field::Mask => {
                out.write_char('[')?;
                for (i, lane) in o.mask().iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    write!(out, "{lane}")?;
                }
                out.write_char(']')?;
            }
            Field::Const => const_hex(o.constant(), out)?,
            Field::Cases => {
                out.write_char('[')?;
                let scrutinee = scrutinee.unwrap_or(crate::func::Value(0));
                for (i, (words, to)) in o.cases(scrutinee).enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    const_hex(words, out)?;
                    out.write_str(": ")?;
                    target(to, out)?;
                }
                out.write_char(']')?;
            }
        }
    }
    if let Some(row) = view.attrs[at].checked_sub(1) {
        write!(out, " !m{row}")?;
    }
    if let Some(row) = view.locs[at].checked_sub(1) {
        write!(out, " @sp{row}")?;
    }
    Ok(())
}

/// [`print`] as a value to format.
pub struct Printed<'a>(pub View<'a>);

impl fmt::Display for Printed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write(self.0, f)
    }
}
