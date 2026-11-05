//! Position-independent code: `.abicalls`, the `$gp` setup directives, and
//! the data directives that need a relocation `.word` cannot spell.
//!
//! MIPS reaches everything outside the object through a global offset table
//! that `$gp` points into, and the ABI leaves setting `$gp` up to the
//! function prologue. GNU as has a directive per shape of prologue, each
//! standing for a short sequence of real instructions, and which of them do
//! anything depends on the ABI: o32 loads `$gp` from `_gp_disp` and the
//! caller's `$t9` (`.cpload`) and saves it across a call on the stack
//! (`.cprestore`), while n32 and n64 compute it from the function's own
//! address (`.cpsetup`) and put it back afterwards (`.cpreturn`). A
//! directive that belongs to the other ABI is not an error in either
//! reference: it is read and ignored, and it is here too.
//!
//! None of them does anything until the file has said `.abicalls`, or
//! `.option pic2`, which is the same thing. That is GNU as's rule — its
//! `mips_pic` starts at `NO_PIC` for an ELF target given no `-KPIC` — and
//! quietly ignoring a `.cpload` in a file that never asked for
//! position-independent code is what both references do. rsasm has no
//! `-KPIC` of its own, so `.abicalls` is the only way in.
//!
//! # `.set noreorder` and `.set nomacro`
//!
//! Neither changes what these directives expand to. GNU as warns
//! ".cpload not in noreorder section" for a `.cpload` outside
//! `.set noreorder`, because the three instructions it writes must not have
//! a branch delay slot between them; this backend behaves as `noreorder`
//! always, as the note on [`super`] says, so the warning has nothing to say
//! here. `.set nomacro` makes GNU as warn "macro instruction
//! expanded into multiple instructions" about each of these and about `li`
//! and `la`, and emit the same bytes either way, which is why rsasm accepts
//! the option and goes on expanding.
//!
//! # What is not here
//!
//! `.abicalls` also changes how GNU as expands `la`, `j` and `jal`: each
//! reaches its symbol through the GOT instead, in a sequence that differs
//! by ABI, by whether the symbol is local, and by whether a `.cprestore`
//! has been seen, and `j` of a symbol stops being a jump at all. rsasm
//! expands none of that and refuses the three rather than assemble the
//! direct form, which would be a different program.

use super::encode::{Words, field_hi16, field_imm16, imm, rd, rs, rt};
use super::operand::OperandParser;
use super::reg::{self, Reg};
use super::reloc;
use super::{FEATURE_PIC, Mips};
use crate::arch::{AsmCtx, Request};
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::lexer::Punct;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// The opcodes the expansions are built from.
const LUI: u32 = 0x0f << 26;
const ADDIU: u32 = 0x09 << 26;
const SW: u32 = 0x2b << 26;
const SD: u32 = 0x3f << 26;
const LD: u32 = 0x37 << 26;
/// SPECIAL function codes.
const OR: u32 = 0x25;
const ADDU: u32 = 0x21;
const DADDU: u32 = 0x2d;

/// What one `$gp` directive leaves for the next, kept in
/// [`ArchState::private`](crate::arch::ArchState::private).
///
/// `.cplocal` says which register holds `$gp`, and `.cpsetup` says where it
/// put the caller's, which is the only thing `.cpreturn` reads. A zero word
/// is the state GNU as starts in: `$gp` itself, and an offset of -1, which
/// is a sentinel it never checks — a `.cpreturn` with no `.cpsetup` in
/// front of it really does load from `-1($sp)`.
#[derive(Copy, Clone)]
struct GpState {
    /// The register holding `$gp`, which `.cplocal` changes.
    gp: Reg,
    /// Where `.cpsetup` put the caller's `$gp`.
    save: CpSave,
}

#[derive(Copy, Clone)]
enum CpSave {
    /// At this offset from `$sp`, as it goes into a 16-bit field.
    Offset(u16),
    Register(Reg),
}

impl GpState {
    fn load(private: u64) -> GpState {
        if private & 1 == 0 {
            return GpState {
                gp: reg::GP,
                save: CpSave::Offset(0xffff),
            };
        }
        let save = if (private >> 6) & 1 != 0 {
            CpSave::Register(Reg::gpr(((private >> 7) & 0x1f) as u8))
        } else {
            CpSave::Offset((private >> 12) as u16)
        };
        GpState {
            gp: Reg::gpr(((private >> 1) & 0x1f) as u8),
            save,
        }
    }

    fn store(self, private: &mut u64) {
        let (flag, reg, off) = match self.save {
            CpSave::Register(r) => (1, u64::from(r.num), 0),
            CpSave::Offset(o) => (0, 0, u64::from(o)),
        };
        *private = 1 | (u64::from(self.gp.num) << 1) | (flag << 6) | (reg << 7) | (off << 12);
    }
}

/// True once the file has said `.abicalls` or `.option pic2`.
fn pic(cx: &AsmCtx<'_>) -> bool {
    cx.state.features & FEATURE_PIC != 0
}

/// Queues the words a directive stands for.
fn emit(cx: &mut AsmCtx<'_>, w: Words) {
    cx.requests.push(Request::Emit {
        bytes: w.finish(),
        code: true,
    });
}

impl Mips {
    /// One of the directives this module is about, or `false` where the
    /// name is not one of them.
    pub(crate) fn pic_directive(
        &self,
        cx: &mut AsmCtx<'_>,
        name: &str,
        cur: &mut Cursor<'_>,
        span: Span,
    ) -> bool {
        // The new ABIs are the ones whose `$gp` is a function's own
        // business; o32 has the caller hand it over in `$t9`.
        let newabi = self.bits == 64;
        // A directive of the other ABI, or any of them before the file has
        // asked for position-independent code, is read and does nothing.
        let ignored = |d: &str| match d {
            ".cpload" | ".cprestore" => newabi || !pic(cx),
            ".cpsetup" | ".cpreturn" | ".cplocal" => !newabi || !pic(cx),
            _ => false,
        };
        match name {
            ".abicalls" => cx.state.features |= FEATURE_PIC,
            ".option" => option(cx, cur),
            _ if ignored(name) => skip(cur),
            ".cpload" => self.cpload(cx, cur, span),
            ".cprestore" => self.cprestore(cx, cur, span),
            ".cpsetup" => self.cpsetup(cx, cur),
            ".cpreturn" => self.cpreturn(cx),
            ".cplocal" => self.cplocal(cx, cur),
            ".gpword" => self.gp_data(cx, cur, span, 4),
            ".gpdword" => self.gp_data(cx, cur, span, 8),
            ".dtprelword" => tls_data(cx, cur, 4, reloc::TLS_DTPREL32),
            ".dtpreldword" => tls_data(cx, cur, 8, reloc::TLS_DTPREL64),
            ".tprelword" => tls_data(cx, cur, 4, reloc::TLS_TPREL32),
            // `R_MIPS_TLS_TPREL64` is the one of the four GNU as 2.47
            // cannot write: it stops with an internal error rather than
            // assemble one, so nothing says what the eight bytes hold.
            ".tpreldword" => {
                cx.error(
                    span,
                    "`.tpreldword` is refused: GNU as 2.47 stops with an internal error \
                     on it, so no reference says what the eight bytes should hold",
                );
                skip(cur);
            }
            _ => return false,
        }
        true
    }

    /// `.cpload $reg`, o32's prologue:
    ///
    /// ```text
    /// lui   $gp, %hi(_gp_disp)
    /// addiu $gp, $gp, %lo(_gp_disp)
    /// addu  $gp, $gp, $reg
    /// ```
    ///
    /// `_gp_disp` is the distance from the `lui` to the linker's `_gp`, and
    /// `$reg` — `$t9` by convention — holds the function's own address, so
    /// the three together give the GOT's address whatever the code was
    /// loaded at.
    fn cpload(&self, cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>, span: Span) {
        let Some(from) = (OperandParser { cx }).gp_register(cur) else {
            skip(cur);
            return;
        };
        let gp = GpState::load(cx.state.private).gp;
        let name = cx.interner.intern("_gp_disp");
        let disp = cx.exprs.alloc(crate::expr::ExprKind::Sym(name), span);
        super::abi::mark(cx.state, gp);
        super::abi::mark(cx.state, from);
        let mut w = Words::new(self.endian);
        w.push_fixup(
            LUI | rt(gp.num),
            disp,
            FixupKind::data(4)
                .with_reloc(reloc::HI16)
                .scatter(field_hi16),
            span,
        );
        w.push_fixup(
            ADDIU | rt(gp.num) | rs(gp.num),
            disp,
            FixupKind::data(4)
                .with_reloc(reloc::LO16)
                .scatter(field_imm16),
            span,
        );
        w.push(rd(gp.num) | rs(gp.num) | rt(from.num) | ADDU);
        emit(cx, w);
    }

    /// `.cprestore offset`, which saves `$gp` where a call can find it
    /// again: `sw $gp, offset($sp)`, or, for an offset no 16-bit field
    /// holds, the same store through `$at`.
    fn cprestore(&self, cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>, span: Span) {
        let Some(off) = absolute(cx, cur, ".cprestore") else {
            return;
        };
        let gp = GpState::load(cx.state.private).gp;
        super::abi::mark(cx.state, gp);
        super::abi::mark(cx.state, reg::SP);
        let mut w = Words::new(self.endian);
        if (-0x8000..0x8000).contains(&off) {
            w.push(SW | rt(gp.num) | rs(reg::SP.num) | imm(off));
        } else {
            // GNU as's `macro_build_ldst_constoffset`, which warns rather
            // than refuse where the offset does not reach 32 bits either.
            if off.wrapping_add(0x8000) as i32 as i64 != off.wrapping_add(0x8000) {
                cx.diags.warning(
                    span,
                    "this `.cprestore` offset overflows the 32 bits its two instructions \
                     hold, and the high bits are dropped",
                );
            }
            super::abi::mark(cx.state, reg::AT);
            w.push(LUI | rt(reg::AT.num) | imm(off.wrapping_add(0x8000) >> 16));
            w.push(rd(reg::AT.num) | rs(reg::AT.num) | rt(reg::SP.num) | ADDU);
            w.push(SW | rt(gp.num) | rs(reg::AT.num) | imm(off));
        }
        emit(cx, w);
    }

    /// `.cpsetup $reg1, offset|$reg2, label`, the n32 and n64 prologue:
    ///
    /// ```text
    /// sd    $gp, offset($sp)     or   move  $reg2, $gp
    /// lui   $gp, %hi(%neg(%gp_rel(label)))
    /// addiu $gp, $gp, %lo(%neg(%gp_rel(label)))
    /// daddu $gp, $gp, $reg1
    /// ```
    ///
    /// The composite relocation is what the whole nesting exists for: the
    /// linker works out how far `label` is from `_gp`, negates it, and the
    /// two halves added to the function's own address in `$reg1` give `_gp`
    /// back. The middle instruction stays `addiu`, not `daddiu`, in both
    /// references; only the last one widens.
    fn cpsetup(&self, cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
        let Some(from) = (OperandParser { cx }).gp_register(cur) else {
            skip(cur);
            return;
        };
        if !comma(cx, cur) {
            return;
        }
        let save = if cur.check_punct(Punct::Dollar) {
            let Some(r) = (OperandParser { cx }).gp_register(cur) else {
                skip(cur);
                return;
            };
            CpSave::Register(r)
        } else {
            // A `.cpsetup` offset is truncated to the field, as GNU as
            // truncates it: the store is built with a plain `%lo`.
            let Some(off) = absolute(cx, cur, ".cpsetup") else {
                return;
            };
            CpSave::Offset(off as u16)
        };
        if !comma(cx, cur) {
            return;
        }
        let Some(label) = cx.expr_parser().parse(cur) else {
            skip(cur);
            return;
        };
        let gp = GpState::load(cx.state.private).gp;
        GpState { gp, save }.store(&mut cx.state.private);

        super::abi::mark(cx.state, gp);
        super::abi::mark(cx.state, from);
        let mut w = Words::new(self.endian);
        match save {
            CpSave::Offset(off) => {
                super::abi::mark(cx.state, reg::SP);
                w.push(SD | rt(gp.num) | rs(reg::SP.num) | u32::from(off));
            }
            CpSave::Register(r) => {
                super::abi::mark(cx.state, r);
                super::abi::mark_zero(cx.state);
                w.push(rd(r.num) | rs(gp.num) | OR);
            }
        }
        let span = cx.exprs.span(label);
        w.push_fixup(LUI | rt(gp.num), label, neg_gp_rel(true), span);
        w.push_fixup(
            ADDIU | rt(gp.num) | rs(gp.num),
            label,
            neg_gp_rel(false),
            span,
        );
        w.push(rd(gp.num) | rs(gp.num) | rt(from.num) | DADDU);
        emit(cx, w);
    }

    /// `.cpreturn`, which undoes the `.cpsetup` before it: `ld $gp,
    /// offset($sp)` where that saved `$gp` on the stack, `move $gp, $reg2`
    /// where it saved it in a register.
    fn cpreturn(&self, cx: &mut AsmCtx<'_>) {
        let st = GpState::load(cx.state.private);
        super::abi::mark(cx.state, st.gp);
        let mut w = Words::new(self.endian);
        match st.save {
            CpSave::Offset(off) => {
                super::abi::mark(cx.state, reg::SP);
                w.push(LD | rt(st.gp.num) | rs(reg::SP.num) | u32::from(off));
            }
            CpSave::Register(r) => {
                super::abi::mark(cx.state, r);
                super::abi::mark_zero(cx.state);
                w.push(rd(st.gp.num) | rs(r.num) | OR);
            }
        }
        emit(cx, w);
    }

    /// `.cplocal $reg`, which says another register holds `$gp` from here
    /// on. It emits nothing; the directives after it use the register it
    /// named.
    fn cplocal(&self, cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
        let Some(r) = (OperandParser { cx }).gp_register(cur) else {
            skip(cur);
            return;
        };
        let st = GpState::load(cx.state.private);
        GpState { gp: r, ..st }.store(&mut cx.state.private);
    }

    /// `.gpword sym` and `.gpdword sym`: the distance from `_gp` to `sym`,
    /// in four bytes or eight, which is how a position-independent jump
    /// table holds its entries. Outside position-independent code they are
    /// `.word` and an eight-byte word, as GNU as makes them, and take a
    /// list like any other data directive.
    ///
    /// In position-independent code neither takes an addend: GNU as calls
    /// anything but a bare symbol an unsupported use. Eight bytes is
    /// `GPREL32` composed with `R_MIPS_64`, which only an n64 `r_info` has
    /// room for — in an o32 object GNU as writes two separate entries for
    /// the one field, which no linker reads, so that spelling is refused.
    ///
    /// Neither is aligned here. GNU as rounds `.gpword` up to four bytes
    /// and `.gpdword` to eight, as it rounds `.word` up on this target;
    /// llvm-mc aligns none of the three, and rsasm follows llvm-mc for
    /// MIPS data as the README's Verification section says.
    fn gp_data(&self, cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>, span: Span, size: u8) {
        if !pic(cx) {
            let reloc = if size == 4 { reloc::R32 } else { reloc::R64 };
            loop {
                let Some(e) = cx.expr_parser().parse(cur) else {
                    skip(cur);
                    return;
                };
                data_word(cx, e, size, FixupKind::data(size).with_reloc(reloc));
                if cur.eat_punct(Punct::Comma).is_none() {
                    return;
                }
            }
        }
        if size == 8 && self.bits != 64 {
            cx.error(
                span,
                "`.gpdword` needs the n64 `r_info`, which holds the two relocation types \
                 it takes; in an o32 object GNU as writes two entries for the one field, \
                 which no linker reads",
            );
            skip(cur);
            return;
        }
        let Some(e) = cx.expr_parser().parse(cur) else {
            skip(cur);
            return;
        };
        // GNU as calls anything but a bare symbol an unsupported use of the
        // directive, an addend included: the field holds the whole distance
        // from `_gp`, so there is nowhere to keep one under `REL`.
        if !matches!(
            cx.exprs.get(e).kind,
            crate::expr::ExprKind::Sym(_)
                | crate::expr::ExprKind::SymId(_)
                | crate::expr::ExprKind::LocalRef(..)
        ) {
            cx.error(
                span,
                "a gp-relative word takes a bare symbol: neither a number, whose \
                 distance from `_gp` nothing can work out, nor a symbol with an \
                 addend, which the field has no room for",
            );
            return;
        }
        let types: &[u32] = if size == 4 {
            &[reloc::GPREL32]
        } else {
            &[reloc::GPREL32, reloc::R64]
        };
        data_word(
            cx,
            e,
            size,
            FixupKind::data(size)
                .with_reloc(reloc::compose(types))
                .linker_only(),
        );
    }
}

/// `.option pic0` and `.option pic2`, which say what `-mno-abicalls` and
/// `.abicalls` say. GNU as also reads `.option O...`, an optimisation
/// setting its own source calls a FIXME and acts on nowhere, refuses any
/// other `pic` number, and warns about anything else.
fn option(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
    let tok = cur.peek();
    let Some(n) = tok.ident() else {
        cx.error(tok.span, "`.option` needs a name");
        skip(cur);
        return;
    };
    let word = cx.name(n).to_string();
    skip(cur);
    match word.as_str() {
        "pic0" => cx.state.features &= !FEATURE_PIC,
        "pic2" => cx.state.features |= FEATURE_PIC,
        // GNU as reads `pic` and one digit, and refuses any number but 0
        // and 2; anything else is an option it does not know.
        _ if word.len() == 4 && word.starts_with("pic") && word.as_bytes()[3].is_ascii_digit() => {
            cx.error(tok.span, format!("`.option {word}` is not supported"));
        }
        _ if word.starts_with('O') => {}
        _ => cx.diags.warning(
            tok.span,
            format!("`.option {word}` is not an option this backend knows"),
        ),
    }
}

/// `.dtprelword`, `.dtpreldword` and `.tprelword`: a variable's offset
/// within its module's thread-local block, or from the thread pointer, as a
/// whole word. A `.word` cannot spell either, since both references refuse
/// an access-model operator in one.
fn tls_data(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>, size: u8, reloc: u32) {
    let Some(e) = cx.expr_parser().parse(cur) else {
        skip(cur);
        return;
    };
    data_word(
        cx,
        e,
        size,
        FixupKind::data(size)
            .with_reloc(reloc)
            .with_class(crate::reloc::RelocClass::ThreadLocal)
            .linker_only(),
    );
}

/// Queues `size` zero bytes with one fixup over them, which is what every
/// data directive here writes.
fn data_word(cx: &mut AsmCtx<'_>, expr: ExprRef, size: u8, kind: FixupKind) {
    let span = cx.exprs.span(expr);
    cx.requests.push(Request::Emit {
        bytes: Variant {
            bytes: vec![0; size as usize],
            fixups: vec![Fixup {
                offset: 0,
                expr,
                kind,
                span,
            }],
        },
        code: false,
    });
}

/// The fixup of one half of `.cpsetup`'s `%hi(%neg(%gp_rel(label)))`, whose
/// three relocation types an n64 `r_info` holds at once.
fn neg_gp_rel(high: bool) -> FixupKind {
    let narrow = if high { reloc::HI16 } else { reloc::LO16 };
    FixupKind::data(4)
        .with_reloc(reloc::compose(&[reloc::GPREL16, reloc::SUB, narrow]))
        .linker_only()
        .scatter(if high { field_hi16 } else { field_imm16 })
}

/// Reads the `,` between two operands, with GNU as's own message where it
/// is missing.
fn comma(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) -> bool {
    if cur.eat_punct(Punct::Comma).is_some() {
        return true;
    }
    cx.error(
        cur.peek().span,
        "missing argument separator `,` for `.cpsetup`",
    );
    skip(cur);
    false
}

/// An expression that has to be a number, which is what GNU as's
/// `get_absolute_expression` reads here.
fn absolute(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>, what: &str) -> Option<i64> {
    let tok = cur.peek();
    let e = cx.expr_parser().parse(cur)?;
    match cx.constant(e) {
        Some(n) => Some(n),
        None => {
            cx.error(tok.span, format!("`{what}` needs a constant offset"));
            skip(cur);
            None
        }
    }
}

/// Swallows the rest of the statement, so that one bad operand does not
/// become a second diagnostic about the tokens after it.
fn skip(cur: &mut Cursor<'_>) {
    cur.set_pos(cur.all().len());
}
