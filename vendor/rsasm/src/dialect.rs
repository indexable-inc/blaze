//! Vendor dialect directives: the bare words Motorola and Renesas assemblers
//! use where GNU as has dotted directives.
//!
//! Most are an existing GNU as directive under another name and are handed
//! straight to it. The rest have no GNU as equivalent — `even`, `cnop`, the
//! sized `ds.w` — or take their arguments in a different shape, and get a
//! small handler here.
//!
//! Every behaviour below was checked against a reference: vasm (in Devpac
//! compatibility, `-no-opt -devpac`) and GNU as `--mri` for Motorola.
//!
//! The CC-RL, CC-RH and CC-RX dialects dot their directives and have far
//! more of them; their tables and handlers are in [`crate::dialect_cc`].
//!
//! ## The 8-bit dialect
//!
//! [`Dialect::EightBit`] is the union of what four references read, each for
//! the source it is the usual assembler of: cc65's ca65 for the 6502, GNU as
//! and vasm's `oldstyle` syntax for the Z80 (vasm for the 6502 too), and the
//! Macro Assembler AS for the 8080 in Intel mnemonics. Their directive sets
//! overlap without contradicting each other, so one table serves: `DB`,
//! `DEFB`, `.byte`, `DW`, `DEFW`, `.word`, `DS`, `DEFS`, `.res`, `ORG`,
//! `EQU`, `END`. Where they do disagree the choice is written down here:
//!
//! - **`ORG` says where the code is, and pads only between pieces.** The
//!   first `ORG` in a section, before anything is emitted, is its load
//!   address, so `ORG 100H` does not put 256 zero bytes in front of a CP/M
//!   program; vasm, AS and ca65 all agree. A later `ORG` pads up to the
//!   address, as vasm and AS do in a binary image; ca65 moves the location
//!   counter without padding. GNU as pads from the section start for every
//!   `.org`, which is what an ELF object needs, and is what the 8-bit
//!   dialect does in relocatable output too.
//! - **A first-column word is a label unless it names an instruction or a
//!   directive,** which reads both vasm's and AS's colonless labels and ca65's
//!   first-column instructions; see `Builder::build` in [`crate::parser`].
//! - **No bare directive is a 6502, Z80 or 8080 mnemonic.** `RES` is a Z80
//!   instruction, so ca65's `res` needs its dot, and `SET` defines a name
//!   only on a target that has no `SET` instruction.
//! - **The location counter in a data list is each item's address,** as in
//!   ca65, vasm and GNU as; AS keeps the statement's.
//! - **A comparison is 1 when true,** as in ca65 and AS; GNU as and vasm give
//!   -1. Operators bind as in C, where ca65 puts `&` with `*`.
//! - **`.align` counts bytes,** as in ca65. vasm's `align` counts bits.
//! - **On the 8051, `.` is a bit number and `BIT` defines a name,** as in AS:
//!   `P1.3` is one operand, and `BIT`, `DATA`, `IDATA`, `XDATA`, `CODE`,
//!   `SFR` and `SFRB` work as `EQU` does; see `arch::retro::mcs51`.
//! - **Only dotted names switch segments.** ca65's `.code`, `.data`,
//!   `.rodata`, `.bss`, `.zeropage` and `.segment "NAME"` do; the bare word
//!   `data` is vasm's byte directive in other assemblers and is not one here.

use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::dialect_cc::CcDirective;
use crate::lexer::{Dialect, Punct, TokKind};
use crate::parser::{Body, Statement};
use crate::section::{FragKind, Fragment, SectionFlags, SectionKind};
use crate::source::Span;

/// What a bare vendor word means.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Alias {
    /// A GNU as directive under another name; the arguments line up.
    Gas(&'static str),
    /// Emit values `width` bytes wide: `dc.w`, `DB`.
    Data(u8),
    /// Reserve `count * width` bytes: `ds.w 3`, `DS 10`.
    Space(u8),
    /// `count` copies of a `width`-byte value: `dcb.w 3, $abcd`.
    Fill(u8),
    /// Align to the architecture's natural unit, as `even` does.
    Even,
    /// `cnop offset, align`: align to `align`, then add `offset`.
    Cnop,
    /// Stop assembling this file.
    End,
    /// Names defined elsewhere: `xref`, `EXTRN`. Undefined symbols are already
    /// external, so this only records the names.
    Extern,
    /// Accepted and ignored: `NAME` (a module name).
    Ignore,
    /// A Motorola `section name[,type]`.
    MotorolaSection,
    /// A Renesas `CSEG` / `DSEG` / `BSEG`.
    Segment(SectionKind, SectionFlags, &'static str),
    /// A CC-RL, CC-RH or CC-RX directive with a handler of its own.
    Cc(CcDirective),
    /// The 8-bit dialect's `ORG`; see the module comment.
    Origin,
    /// ca65's `.segment "NAME"`.
    NamedSegment,
    /// One byte per value, the value shifted right by this many bits first:
    /// ca65's `.lobytes`, `.hibytes` and `.bankbytes`.
    ByteOf(u8),
    /// ca65's `.dbyt`: a 16-bit value, high byte first.
    BigWord,
    /// ca65's `.setcpu` and `.p02`, which choose the instruction set.
    SetCpu,
}

/// Looks up `word` — already lowercased, with any leading dot removed — in a
/// dialect's directive table.
pub(crate) fn lookup(dialect: Dialect, word: &str) -> Option<Alias> {
    use Alias::*;
    if dialect.renesas_cc() {
        return crate::dialect_cc::lookup(dialect, word);
    }
    if dialect == Dialect::EightBit {
        return Some(match word {
            // `DEFM` is Zilog's string form of `DEFB`, and takes values too.
            "db" | "defb" | "defm" | "byte" | "byt" | "dfb" => Data(1),
            "dw" | "defw" | "word" | "dfw" | "addr" => Data(2),
            "ds" | "defs" | "dsb" | "blk" => Gas(".space"),
            "org" => Origin,
            "align" => Gas(".balign"),
            "include" => Gas(".include"),
            "incbin" => Gas(".incbin"),
            "if" => Gas(".if"),
            "ifdef" => Gas(".ifdef"),
            "ifndef" => Gas(".ifndef"),
            "else" => Gas(".else"),
            "elseif" => Gas(".elseif"),
            "endif" => Gas(".endif"),
            "end" => End,
            "public" | "global" | "xdef" => Gas(".globl"),
            "extern" | "extrn" | "xref" => Extern,
            _ => return None,
        });
    }
    let common = match word {
        "org" => Some(Gas(".org")),
        "include" => Some(Gas(".include")),
        "incbin" => Some(Gas(".incbin")),
        "if" => Some(Gas(".if")),
        "else" => Some(Gas(".else")),
        "elseif" => Some(Gas(".elseif")),
        "endif" => Some(Gas(".endif")),
        "end" => Some(End),
        _ => None,
    };
    if common.is_some() {
        return common;
    }
    match dialect {
        Dialect::Motorola => Some(match word {
            "dc" | "dc.w" => Data(2),
            "dc.b" => Data(1),
            "dc.l" => Data(4),
            "ds" | "ds.w" => Space(2),
            "ds.b" => Space(1),
            "ds.l" => Space(4),
            "dcb" | "dcb.w" => Fill(2),
            "dcb.b" => Fill(1),
            "dcb.l" => Fill(4),
            "even" => Even,
            "cnop" => Cnop,
            "section" => MotorolaSection,
            "xdef" | "global" | "public" => Gas(".globl"),
            "xref" => Extern,
            "ifeq" => Gas(".ifeq"),
            "ifne" => Gas(".ifne"),
            "ifd" => Gas(".ifdef"),
            "ifnd" => Gas(".ifndef"),
            // Devpac's name for the end of a conditional.
            "endc" => Gas(".endif"),
            _ => return None,
        }),
        Dialect::Renesas => Some(match word {
            "db" => Data(1),
            "dw" => Data(2),
            "ds" => Space(1),
            "public" => Gas(".globl"),
            "extrn" | "extern" => Extern,
            "name" => Ignore,
            "cseg" => Segment(SectionKind::Progbits, SectionFlags::text(), ".text"),
            "dseg" => Segment(SectionKind::Progbits, SectionFlags::data(), ".data"),
            "bseg" => Segment(SectionKind::Nobits, SectionFlags::bss(), ".bss"),
            _ => return None,
        }),
        Dialect::Gas
        | Dialect::Nasm
        | Dialect::CcRl
        | Dialect::CcRh
        | Dialect::CcRx
        | Dialect::EightBit => None,
    }
}

/// The 8-bit dialect's directives that exist only with a leading dot, as ca65
/// spells them. `word` has the dot removed and is lowercased.
pub(crate) fn lookup_dotted(dialect: Dialect, word: &str) -> Option<Alias> {
    use Alias::*;
    if dialect != Dialect::EightBit {
        return None;
    }
    Some(match word {
        "code" => Gas(".text"),
        "data" => Gas(".data"),
        "rodata" => Gas(".rodata"),
        "bss" => Gas(".bss"),
        "zeropage" => Segment(SectionKind::Progbits, SectionFlags::data(), "ZEROPAGE"),
        "segment" => NamedSegment,
        "asciiz" => Gas(".asciz"),
        "dbyt" => BigWord,
        "dword" => Data(4),
        // Only dotted: `RES` is a Z80 instruction.
        "res" => Gas(".space"),
        "lobytes" => ByteOf(0),
        "hibytes" => ByteOf(8),
        "bankbytes" => ByteOf(16),
        "export" | "exportzp" | "globalzp" => Gas(".globl"),
        "import" | "importzp" => Extern,
        "setcpu" | "p02" => SetCpu,
        _ => return lookup(dialect, word),
    })
}

/// The block constructs, which the statement walker has to recognise before
/// any directive runs because they consume the statements after them. Returns
/// the GNU as spelling the walker already understands.
///
/// CC-RL and CC-RH end `.REPT` and `.IRP` with `.ENDM` too (CC-RL pages
/// 529-530, CC-RH pages 463-464); the walker knows to look for it.
pub(crate) fn block_keyword(dialect: Dialect, word: &str) -> Option<&'static str> {
    if dialect.is_cc() {
        return Some(match word {
            "macro" => ".macro",
            "endm" => ".endm",
            "exitm" => ".exitm",
            "rept" => ".rept",
            "irp" => ".irp",
            _ => return None,
        });
    }
    // CC-RX ends a repeat with `.ENDR` (R20UT3248EJ0115 pages 488-489).
    if dialect == Dialect::CcRx {
        return Some(match word {
            "macro" => ".macro",
            "endm" => ".endm",
            "exitm" => ".exitm",
            "mrepeat" => ".rept",
            "endr" => ".endr",
            _ => return None,
        });
    }
    if !matches!(
        dialect,
        Dialect::Motorola | Dialect::Renesas | Dialect::EightBit
    ) {
        return None;
    }
    Some(match word {
        "macro" => ".macro",
        // ca65's spellings.
        "endmacro" | "endmac" if dialect == Dialect::EightBit => ".endm",
        "repeat" if dialect == Dialect::EightBit => ".rept",
        "endrep" | "endrepeat" if dialect == Dialect::EightBit => ".endr",
        "endm" => ".endm",
        "exitm" | "mexit" => ".exitm",
        "rept" => ".rept",
        "endr" => ".endr",
        "irp" => ".irp",
        "irpc" => ".irpc",
        _ => return None,
    })
}

/// Conditional directives, which have to be seen even inside a false branch.
pub(crate) fn is_conditional(dialect: Dialect, word: &str) -> bool {
    matches!(
        lookup(dialect, word),
        Some(
            Alias::Gas(
                ".if" | ".ifeq" | ".ifne" | ".ifdef" | ".ifndef" | ".else" | ".elseif" | ".endif"
            ) | Alias::Cc(CcDirective::ElseIfN)
        )
    )
}

impl Assembler {
    /// Runs a vendor directive. `stmt`'s arguments start just after the word.
    pub(crate) fn run_alias(&mut self, stmt: &Statement, alias: Alias) {
        let span = stmt.span;
        let mut cur = stmt.arg_cursor();
        match alias {
            Alias::Gas(name) => {
                let n = self.interner.intern(name);
                self.builtin_directive(stmt, n);
                return;
            }
            Alias::Data(width) => {
                let word = self.statement_word(stmt);
                self.alias_data(&mut cur, width, span, &word)
            }
            Alias::Space(width) => self.alias_space(&mut cur, width, span),
            Alias::Fill(width) => self.alias_fill(&mut cur, width, span),
            Alias::Even => {
                let unit = self.arch.align_unit().max(2);
                self.align_to(unit, span);
            }
            Alias::Cnop => self.alias_cnop(&mut cur, span),
            Alias::End => {
                self.end_of_source = true;
                cur.set_pos(cur.all().len());
            }
            Alias::Extern => loop {
                let tok = cur.peek();
                let Some(name) = tok.ident() else {
                    self.diags.error(tok.span, "expected a symbol name");
                    return;
                };
                cur.advance();
                self.symbols.intern(name, tok.span);
                if cur.eat_punct(Punct::Comma).is_none() {
                    break;
                }
            },
            Alias::Ignore => cur.set_pos(cur.all().len()),
            Alias::MotorolaSection => self.alias_motorola_section(&mut cur, span),
            Alias::Segment(kind, flags, section) => {
                let name = self.interner.intern(section);
                let id = self.get_or_create_section(name, kind, flags, 1);
                self.set_section(id);
                // Relocation attributes (`CSEG AT 1000H`, `CSEG UNIT`) are not
                // modelled; they place the segment, which is the linker's job.
                cur.set_pos(cur.all().len());
            }
            Alias::Cc(d) => {
                self.run_cc(stmt, d);
                return;
            }
            Alias::Origin => {
                if let Some(e) = self.parse_expr(&mut cur) {
                    self.origin(e, span);
                }
            }
            Alias::NamedSegment => self.alias_named_segment(&mut cur),
            Alias::ByteOf(shift) => self.alias_byte_of(&mut cur, shift, span),
            Alias::BigWord => loop {
                let Some(e) = self.parse_expr(&mut cur) else {
                    return;
                };
                let espan = self.exprs.span(e);
                let high = self.exprs.alloc(
                    crate::expr::ExprKind::Unary(crate::expr::UnOp::High, e),
                    espan,
                );
                let low = self.exprs.alloc(
                    crate::expr::ExprKind::Unary(crate::expr::UnOp::Low, e),
                    espan,
                );
                self.emit_value(1, high, span);
                self.emit_value(1, low, span);
                if cur.eat_punct(Punct::Comma).is_none() {
                    break;
                }
            },
            Alias::SetCpu => self.alias_setcpu(stmt, &mut cur),
        }
        self.expect_end(&mut cur);
    }

    /// The 8-bit dialect's `ORG`: the load address of a section that has
    /// nothing in it yet, and padding up to the address after that. See the
    /// module comment.
    pub(crate) fn origin(&mut self, e: crate::expr::ExprRef, span: Span) {
        if self.options.relocatable {
            self.emit_org(e, 0, span);
            return;
        }
        let Some(addr) = self.eval_absolute(e, "an `org` address") else {
            return;
        };
        let Ok(addr) = u64::try_from(addr) else {
            self.diags
                .error(span, format!("`org` address {addr} is negative"));
            return;
        };
        let cur = self.cur;
        let first = cur.0 == 0;
        let base = self.options.base_addr;
        let section = self.section_mut(cur);
        if section.frags.is_empty() && section.origin.is_none() {
            section.origin = Some(addr);
            return;
        }
        // Code before the first `ORG` starts where the image does.
        let start = section.origin.unwrap_or(if first { base } else { 0 });
        let Some(offset) = addr.checked_sub(start) else {
            self.diags.error(
                span,
                format!("`org` {addr:#x} is before the start of the section, {start:#x}"),
            );
            return;
        };
        let target = self.exprs.int(offset, span);
        self.emit_org(target, 0, span);
    }

    /// `.segment "NAME"`. ca65's standard segment names are the sections GNU
    /// as would use for them.
    fn alias_named_segment(&mut self, cur: &mut Cursor<'_>) {
        let tok = cur.peek();
        let TokKind::Str(i) = tok.kind else {
            self.diags
                .error(tok.span, "expected a segment name in double quotes");
            cur.set_pos(cur.all().len());
            return;
        };
        cur.advance();
        let name = String::from_utf8_lossy(self.pool.get(i)).into_owned();
        // An address size (`: zeropage`) may follow; placement is the
        // linker's.
        if cur.eat_punct(Punct::Colon).is_some() {
            cur.set_pos(cur.all().len());
        }
        let id = match name.as_str() {
            "CODE" => self.standard_section(".text"),
            "DATA" => self.standard_section(".data"),
            "RODATA" => self.standard_section(".rodata"),
            "BSS" => self.standard_section(".bss"),
            _ => {
                let n = self.interner.intern(&name);
                self.get_or_create_section(n, SectionKind::Progbits, SectionFlags::data(), 1)
            }
        };
        self.set_section(id);
    }

    fn alias_byte_of(&mut self, cur: &mut Cursor<'_>, shift: u8, span: Span) {
        use crate::expr::{ExprKind, UnOp};
        loop {
            let Some(e) = self.parse_expr(cur) else {
                return;
            };
            let espan = self.exprs.span(e);
            let op = match shift {
                0 => UnOp::Low,
                8 => UnOp::High,
                _ => UnOp::Bank,
            };
            let byte = self.exprs.alloc(ExprKind::Unary(op, e), espan);
            self.emit_value(1, byte, span);
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
    }

    /// `.setcpu "6502"` and `.p02`. Only the NMOS 6502's documented
    /// instructions are implemented, so any other choice is refused rather
    /// than quietly assembling a different instruction set.
    fn alias_setcpu(&mut self, stmt: &Statement, cur: &mut Cursor<'_>) {
        let word = stmt
            .toks
            .get(stmt.args.wrapping_sub(1))
            .and_then(|t| t.ident())
            .map(|n| self.interner.get(n).to_ascii_lowercase())
            .unwrap_or_default();
        if word == ".p02" {
            return;
        }
        let tok = cur.peek();
        let TokKind::Str(i) = tok.kind else {
            self.diags
                .error(tok.span, "expected a CPU name in double quotes");
            cur.set_pos(cur.all().len());
            return;
        };
        cur.advance();
        let name = String::from_utf8_lossy(self.pool.get(i)).into_owned();
        if name != "6502" || self.arch.name() != "6502" {
            self.diags.error(
                tok.span,
                format!(
                    "`.setcpu \"{name}\"` is not supported; only the NMOS 6502's documented \
                     instruction set is"
                ),
            );
        }
    }

    /// Aligns the location counter to `unit` bytes.
    pub(crate) fn align_to(&mut self, unit: u64, span: Span) {
        if unit <= 1 {
            return;
        }
        let exec = self.section(self.cur).flags.exec;
        let fill = if exec { Vec::new() } else { vec![0] };
        self.cur_section().push(Fragment::new(
            FragKind::Align {
                align: unit,
                fill,
                max_skip: None,
                pad: 0,
                nop_state: None,
            },
            span,
        ));
        let cur = self.cur;
        self.section_mut(cur).align = self.section(cur).align.max(unit);
    }

    /// Motorola syntax keeps word and long data on the architecture's natural
    /// boundary, as Devpac and GNU as `--mri` both do. It is the dialect's
    /// rule — GNU as's own m68k syntax aligns nothing — applied with the unit
    /// the architecture asks for, so a Motorola-syntax 6809 is left alone.
    fn motorola_align(&mut self, width: u8, span: Span) {
        if self.options.dialect == Dialect::Motorola && width >= 2 {
            let unit = self.arch.align_unit();
            self.align_to(unit, span);
        }
    }

    /// The word a statement's body starts with, lowercased, which is how a
    /// dialect's own table looked the directive up and how a diagnostic names
    /// it back.
    fn statement_word(&self, stmt: &Statement) -> String {
        let name = match stmt.body {
            Some(Body::Directive { name, .. } | Body::Insn { mnemonic: name, .. }) => name,
            _ => return String::new(),
        };
        self.interner.get(name).to_ascii_lowercase()
    }

    fn alias_data(&mut self, cur: &mut Cursor<'_>, width: u8, span: Span, directive: &str) {
        self.motorola_align(width, span);
        if cur.at_end() {
            return;
        }
        if self.options.dialect.is_cc() {
            self.cc_data(cur, width, span);
            return;
        }
        if self.options.dialect == Dialect::Renesas && self.renesas_size_form(cur, width, span) {
            return;
        }
        loop {
            // A string is a run of bytes, where the dialect allows one: vasm
            // takes `dc.b "hi",0`, and so does every Renesas `DB`. Single
            // quotes make a string too when the literal stands alone —
            // `dc.b 'text',0` is how Motorola source spells it, and GNU as
            // `--mri` takes no other — while `'AB'+1` stays a number.
            if let TokKind::Str(i) = cur.peek().kind
                && width == 1
            {
                cur.advance();
                let bytes = self.pool.get(i).to_vec();
                self.emit_bytes(&bytes, span);
            } else if let Some(bytes) = self.standalone_quoted(cur)
                && width == 1
            {
                cur.advance();
                self.emit_bytes(&bytes, span);
            } else {
                let Some(e) = self.parse_expr(cur) else {
                    return;
                };
                self.emit_value_in(width, e, span, directive);
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
    }

    /// The bytes of a single-quoted literal at the cursor, if it is a whole
    /// data item: followed by a comma or the end of the statement.
    fn standalone_quoted(&self, cur: &Cursor<'_>) -> Option<Vec<u8>> {
        let tok = cur.peek();
        if !matches!(tok.kind, TokKind::Int(_)) {
            return None;
        }
        let next = cur.nth(1);
        let ends_item = next.is_eol() || matches!(next.kind, TokKind::Punct(Punct::Comma));
        if !ends_item {
            return None;
        }
        let inner = self
            .sm
            .span_text(tok.span)
            .strip_prefix('\'')?
            .strip_suffix('\'')?;
        Some(inner.replace("''", "'").into_bytes())
    }

    /// `DB (4)` and `DW (4)`: a parenthesised operand is a count of zeroed
    /// units, not a value (RA78K0 manual, DB and DW directives).
    fn renesas_size_form(&mut self, cur: &mut Cursor<'_>, width: u8, span: Span) -> bool {
        let toks = cur.rest();
        let body: Vec<_> = toks.iter().take_while(|t| !t.is_eol()).collect();
        let (Some(first), Some(last)) = (body.first(), body.last()) else {
            return false;
        };
        if !matches!(first.kind, TokKind::Punct(Punct::LParen))
            || !matches!(last.kind, TokKind::Punct(Punct::RParen))
        {
            return false;
        }
        // The opening parenthesis must close only at the end: `(1)+(2)` is a
        // value.
        let mut depth = 0i32;
        for (i, t) in body.iter().enumerate() {
            match t.kind {
                TokKind::Punct(Punct::LParen) => depth += 1,
                TokKind::Punct(Punct::RParen) => {
                    depth -= 1;
                    if depth == 0 && i + 1 != body.len() {
                        return false;
                    }
                }
                _ => {}
            }
        }
        self.alias_space(cur, width, span);
        true
    }

    pub(crate) fn alias_space(&mut self, cur: &mut Cursor<'_>, width: u8, span: Span) {
        self.motorola_align(width, span);
        let Some(count) = self.parse_expr(cur) else {
            return;
        };
        let size = if width == 1 {
            count
        } else {
            let w = self.exprs.int(width as u64, span);
            self.exprs.alloc(
                crate::expr::ExprKind::Binary(crate::expr::BinOp::Mul, count, w),
                span,
            )
        };
        let fill = self.exprs.int(0, span);
        self.cur_section().push(Fragment::new(
            FragKind::Space {
                size,
                fill,
                resolved: 0,
            },
            span,
        ));
    }

    pub(crate) fn alias_fill(&mut self, cur: &mut Cursor<'_>, width: u8, span: Span) {
        self.motorola_align(width, span);
        let Some(count_e) = self.parse_expr(cur) else {
            return;
        };
        let Some(count) = self.eval_absolute(count_e, "repeat count") else {
            return;
        };
        let value = if cur.eat_punct(Punct::Comma).is_some() {
            match self.parse_expr(cur) {
                Some(e) => self.eval_absolute(e, "fill value").unwrap_or(0),
                None => return,
            }
        } else {
            0
        };
        if !(0..=(1 << 24)).contains(&count) {
            self.diags
                .error(span, format!("repeat count {count} is out of range"));
            return;
        }
        let unit = self.arch.endian().bytes(value as u64, width as usize);
        let mut bytes = Vec::with_capacity(unit.len() * count as usize);
        for _ in 0..count {
            bytes.extend_from_slice(&unit);
        }
        self.emit_bytes(&bytes, span);
    }

    /// `cnop offset, align`: pad to a multiple of `align`, then `offset` more.
    fn alias_cnop(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let Some(off_e) = self.parse_expr(cur) else {
            return;
        };
        let Some(offset) = self.eval_absolute(off_e, "cnop offset") else {
            return;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags
                .error(span, "`cnop` needs an offset and an alignment");
            return;
        }
        let Some(al_e) = self.parse_expr(cur) else {
            return;
        };
        let Some(align) = self.eval_absolute(al_e, "cnop alignment") else {
            return;
        };
        if align <= 0 || !(align as u64).is_power_of_two() {
            self.diags.error(
                span,
                format!("`cnop` alignment {align} is not a power of two"),
            );
            return;
        }
        if offset < 0 {
            self.diags.error(span, "`cnop` offset must not be negative");
            return;
        }
        self.align_to(align as u64, span);
        if offset > 0 {
            let size = self.exprs.int(offset as u64, span);
            let fill = self.exprs.int(0, span);
            self.cur_section().push(Fragment::new(
                FragKind::Space {
                    size,
                    fill,
                    resolved: 0,
                },
                span,
            ));
        }
    }

    /// `section name[,type]`, where `type` is `code`, `data` or `bss`.
    fn alias_motorola_section(&mut self, cur: &mut Cursor<'_>, span: Span) {
        let tok = cur.peek();
        let name = match tok.kind {
            TokKind::Ident(n) => {
                cur.advance();
                self.interner.get(n).to_string()
            }
            TokKind::Str(i) => {
                cur.advance();
                String::from_utf8_lossy(self.pool.get(i)).into_owned()
            }
            _ => {
                self.diags.error(tok.span, "expected a section name");
                return;
            }
        };
        let mut kind = SectionKind::Progbits;
        let mut flags = SectionFlags::text();
        if cur.eat_punct(Punct::Comma).is_some() {
            let tok = cur.peek();
            let Some(t) = tok.ident() else {
                self.diags
                    .error(tok.span, "expected `code`, `data` or `bss`");
                return;
            };
            cur.advance();
            // vasm also accepts a memory attribute after the type (`data_c`,
            // `,chip`); those place the section, which is the linker's job.
            let ty = self.interner.get(t).to_ascii_lowercase();
            match ty.split('_').next().unwrap_or("") {
                "code" | "text" => flags = SectionFlags::text(),
                "data" => flags = SectionFlags::data(),
                "bss" => {
                    kind = SectionKind::Nobits;
                    flags = SectionFlags::bss();
                }
                other => {
                    self.diags.error(
                        tok.span,
                        format!("unknown section type `{other}`; expected `code`, `data` or `bss`"),
                    );
                    return;
                }
            }
            if cur.eat_punct(Punct::Comma).is_some() {
                cur.set_pos(cur.all().len());
            }
        }
        let n = self.interner.intern(&name);
        let id = self.get_or_create_section(n, kind, flags, 1);
        self.set_section(id);
        let _ = span;
    }
}
