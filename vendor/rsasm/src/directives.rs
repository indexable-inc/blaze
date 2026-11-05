//! Built-in assembler directives.
//!
//! Anything the table here does not claim is offered to the current
//! architecture backend, which is how `.code64` and `.intel_syntax` are
//! handled without the core knowing about x86.

use crate::assembler::{Assembler, Cond};
use crate::cursor::Cursor;
use crate::expr::ExprRef;
use crate::intern::Name;
use crate::lexer::{Dialect, Punct, TokKind};
use crate::parser::Statement;
use crate::section::{FragKind, Fragment, SectionFlags, SectionKind};
use crate::source::Span;
use crate::symbol::{Binding, SymType, SymbolValue, Visibility};
use std::path::PathBuf;

impl Assembler {
    pub(crate) fn directive(&mut self, stmt: &Statement, name: Name) {
        let text = self.interner.get(name).to_string();

        // Renesas's newer assemblers dot their directives (`.DB`, `.CSEG`).
        // Those are the vendor table's words, and take priority over a GNU as
        // directive of the same name, which would read their arguments wrongly.
        //
        // `.section` is the one word where that goes the other way. Neither
        // Motorola reference reads a dotted directive at all — GNU as `--mri`
        // calls it an unknown operator and vasm an unknown mnemonic — so the
        // dot can only be GNU as's own spelling, whose flag string and type
        // (`.section .tbss,"awT",@nobits`) the vendor `SECTION` would refuse.
        if matches!(self.options.dialect, Dialect::Motorola | Dialect::Renesas)
            && let Some(bare) = text.strip_prefix('.')
            && let Some(alias) = crate::dialect::lookup(self.options.dialect, bare)
            && !matches!(
                alias,
                crate::dialect::Alias::Gas(_) | crate::dialect::Alias::MotorolaSection
            )
        {
            self.run_alias(stmt, alias);
            return;
        }
        // In the 8-bit dialect a dotted word is ca65's, whose meaning can
        // differ from the GNU as directive of the same name (`.org`).
        if let Some(bare) = text.strip_prefix('.')
            && let Some(alias) = crate::dialect::lookup_dotted(self.options.dialect, bare)
        {
            self.run_alias(stmt, alias);
            return;
        }
        // CC-RL, CC-RH and CC-RX have tables of their own; the `$` control
        // instructions of the first two keep the `$` in their name.
        if self.options.dialect.renesas_cc()
            && let Some(alias) = crate::dialect::lookup(
                self.options.dialect,
                text.strip_prefix('.').unwrap_or(&text),
            )
        {
            self.run_alias(stmt, alias);
            return;
        }
        self.builtin_directive(stmt, name);
    }

    /// Runs a directive from the GNU as table, or the backend's.
    pub(crate) fn builtin_directive(&mut self, stmt: &Statement, name: Name) {
        let text = self.interner.get(name).to_string();
        let span = stmt.span;
        let mut cur = stmt.arg_cursor();

        // A Mach-O object has directives of its own, and gives some of the
        // common ones another meaning.
        if self.options.format == crate::output::Format::MachO
            && self.macho_directive(&text, &mut cur, span)
        {
            self.expect_end(&mut cur);
            return;
        }

        let handled = match text.as_str() {
            // ---- COFF ------------------------------------------------------
            // First, because `.type` means one thing on its own and another
            // between `.def` and `.endef`.
            _ if crate::coff::is_directive(&text)
                || (self.coff.in_def() && crate::coff::is_def_field(&text)) =>
            {
                self.coff_directive(&text, &mut cur, span)
            }

            // ---- sections -------------------------------------------------
            ".text" | ".data" | ".bss" | ".rodata" => {
                let id = self.standard_section(&text);
                self.set_section(id);
                true
            }
            ".section" => self.dir_section(&mut cur, span, false),
            ".pushsection" => self.dir_section(&mut cur, span, true),
            ".popsection" => {
                match self.pop_section() {
                    Some(id) => {
                        if id != self.cur {
                            self.dwarf_section_switch();
                        }
                        self.cur = id
                    }
                    // GNU as warns and carries on, which keeps a file that
                    // pops one section too many assembling.
                    None => self
                        .diags
                        .warning(span, "`.popsection` without a matching `.pushsection`"),
                }
                true
            }
            ".previous" => {
                self.swap_previous();
                true
            }

            // ---- data -----------------------------------------------------
            // The `.Nbyte` spellings are never aligned, even on a target whose
            // other data directives are; see `Architecture::aligns_data`.
            ".byte" => self.dir_data(&mut cur, 1, span, false, &text),
            // `.value` is x86's spelling, which GCC writes in debug sections.
            ".short" | ".hword" | ".half" | ".value" => {
                self.dir_data(&mut cur, 2, span, true, &text)
            }
            ".2byte" => self.dir_data(&mut cur, 2, span, false, &text),
            // `.word` is the one data directive whose width depends on the
            // target, so it asks the backend rather than assuming x86.
            ".word" => {
                let w = self.arch.word_bytes();
                self.dir_data(&mut cur, w, span, true, &text)
            }
            // `.3byte` is the RL78 and RX ports' 24-bit address; like
            // `.dword` below, it means nothing else anywhere.
            ".3byte" => self.dir_data(&mut cur, 3, span, false, &text),
            ".int" | ".long" => self.dir_data(&mut cur, 4, span, true, &text),
            ".4byte" => self.dir_data(&mut cur, 4, span, false, &text),
            // `.dword` (MIPS, RISC-V) and `.xword` (AArch64, SPARC V9) both
            // mean eight bytes; accepting them everywhere is harmless, since
            // neither has a different meaning on any other target.
            ".quad" => self.dir_data(&mut cur, 8, span, true, &text),
            ".8byte" | ".dword" | ".xword" => self.dir_data(&mut cur, 8, span, false, &text),
            // SuperH's GNU as names its unaligned `.word`, `.long` and
            // `.quad` these; they mean nothing to a target that aligns no data.
            ".uaword" if self.arch.aligns_data() => self.dir_data(&mut cur, 2, span, false, &text),
            ".ualong" if self.arch.aligns_data() => self.dir_data(&mut cur, 4, span, false, &text),
            ".uaquad" if self.arch.aligns_data() => self.dir_data(&mut cur, 8, span, false, &text),
            // The Motorola data directives, which GNU as's own table gives
            // every target: `.dc` writes values, `.dcb` repeats one and `.ds`
            // reserves room, with a `.b`, `.w` or `.l` suffix for the width
            // and a word where there is none. They are `cons` and `s_space`
            // under another name, so `.dc.w` aligns exactly where `.short`
            // does and the other two align nothing. The undotted spellings
            // the Motorola dialect reads are in [`crate::dialect`], which
            // aligns them the way Devpac and GNU as `--mri` do; a dotted one
            // in that dialect still goes there. The float suffixes
            // (`.dc.s`, `.ds.x`) are left unknown, since no data directive
            // here writes a float.
            ".dc" | ".dc.b" | ".dc.w" | ".dc.l" => {
                let w = motorola_width(&text);
                self.dir_data(&mut cur, w, span, w > 1, &text)
            }
            ".ds" | ".ds.b" | ".ds.w" | ".ds.l" => {
                self.alias_space(&mut cur, motorola_width(&text), span);
                true
            }
            ".dcb" | ".dcb.b" | ".dcb.w" | ".dcb.l" => {
                self.alias_fill(&mut cur, motorola_width(&text), span);
                true
            }
            ".ascii" => self.dir_ascii(&mut cur, false, span),
            ".asciz" | ".string" | ".asciiz" => self.dir_ascii(&mut cur, true, span),
            ".sleb128" => self.dir_leb(&mut cur, true, span),
            ".uleb128" => self.dir_leb(&mut cur, false, span),
            ".zero" => self.dir_space(&mut cur, span, true),
            ".space" | ".skip" => self.dir_space(&mut cur, span, false),
            ".fill" => self.dir_fill(&mut cur, span),
            ".incbin" => self.dir_incbin(&mut cur, span),

            // ---- layout ---------------------------------------------------
            ".align" => {
                let log2 = self.arch.align_is_log2();
                self.dir_align(&mut cur, span, log2)
            }
            ".balign" => self.dir_align(&mut cur, span, false),
            ".p2align" => self.dir_align(&mut cur, span, true),
            ".org" => {
                if let Some(e) = self.parse_expr(&mut cur) {
                    let fill = self.optional_byte(&mut cur, 0);
                    self.push_frag(
                        FragKind::Org {
                            target: e,
                            fill,
                            size: 0,
                        },
                        span,
                    );
                }
                true
            }

            // ---- symbols --------------------------------------------------
            ".globl" | ".global" => self.dir_binding(&mut cur, Binding::Global, span),
            ".weak" => self.dir_binding(&mut cur, Binding::Weak, span),
            ".local" => self.dir_binding(&mut cur, Binding::Local, span),
            ".hidden" => self.dir_visibility(&mut cur, Visibility::Hidden, span),
            ".protected" => self.dir_visibility(&mut cur, Visibility::Protected, span),
            ".internal" => self.dir_visibility(&mut cur, Visibility::Internal, span),
            // `.set word` with nothing after it is not an assignment: MIPS
            // uses it for assembler options (`.set noreorder`). Only `.set`
            // has that second meaning, so the other spellings go straight
            // to the assignment.
            ".set" if Self::is_set_option(&cur) => false,
            ".set" | ".equ" | ".equiv" => self.dir_set(&mut cur, span, text == ".equiv", 0),
            ".size" => self.dir_size(&mut cur, span),
            ".type" => self.dir_type(&mut cur, span),
            ".comm" => self.dir_comm(&mut cur, span, false, SymType::Object),
            ".lcomm" => self.dir_comm(&mut cur, span, true, SymType::Object),
            // A thread-local common block is an ELF idea; neither the PE nor
            // the Mach-O reference knows the directive, so neither does this.
            ".tls_common" if self.options.format == crate::output::Format::Elf => {
                self.dir_comm(&mut cur, span, false, SymType::Tls)
            }

            // ---- conditionals ---------------------------------------------
            ".if" | ".ifeq" | ".ifne" | ".ifdef" | ".ifndef" | ".ifb" | ".ifnb" => {
                self.dir_if(&mut cur, &text, span)
            }
            ".elseif" | ".elif" => self.dir_elseif(&mut cur, span),
            ".else" => {
                self.dir_else(span);
                true
            }
            ".endif" => {
                if self.pop_cond().is_none() {
                    self.diags.error(span, "`.endif` without a matching `.if`");
                }
                true
            }

            // ---- diagnostics ----------------------------------------------
            ".error" | ".err" => {
                let msg = self
                    .optional_string(&mut cur)
                    .unwrap_or_else(|| ".error directive".into());
                self.diags.error(span, msg);
                true
            }
            ".warning" => {
                let msg = self
                    .optional_string(&mut cur)
                    .unwrap_or_else(|| ".warning directive".into());
                self.diags.warning(span, msg);
                true
            }

            // ---- macros ---------------------------------------------------
            // `.macro`, `.endm`, `.exitm`, `.rept`, `.irp`, `.irpc` and
            // `.endr` never reach here: the statement walker intercepts them
            // because they consume the statements that follow. Only `.purgem`
            // is an ordinary directive.
            ".purgem" => self.dir_purgem(&mut cur),

            // ---- files and configuration ----------------------------------
            ".include" => self.dir_include(&mut cur, span),
            ".arch" | ".cpu" => self.dir_arch(&mut cur, span, text == ".cpu"),
            // ---- debugging information ------------------------------------
            // A COFF object records the source file name as a symbol of its
            // own; the numbered form is DWARF's either way.
            ".file" if self.options.format.is_coff() && !Self::is_numbered_file(&cur) => {
                if let Some(s) = self.expect_string(&mut cur, "a file name") {
                    let name = String::from_utf8_lossy(&s).into_owned();
                    self.coff.files.push(name);
                }
                true
            }
            ".file" => {
                self.dir_dwarf_file(&mut cur, span);
                true
            }
            ".loc" => {
                self.dir_loc(&mut cur, span);
                true
            }
            ".loc_mark_labels" => {
                self.dir_loc_mark_labels(&mut cur, span);
                true
            }
            _ if text.starts_with(".cfi_") => self.dir_cfi(&text, &mut cur, span),
            // ---- build attributes -----------------------------------------
            // Every ELF target has this one: the vendor-neutral attributes,
            // which on PowerPC record the floating-point and vector ABIs a
            // linker refuses to mix. A target's own directive
            // (`.eabi_attribute`, `.attribute`) is the backend's.
            ".gnu_attribute" => self.dir_gnu_attribute(&mut cur, span),

            // Recognised and ignored: they carry no information this assembler
            // acts on yet, and rejecting them would break real-world input.
            ".ident" | ".version" | ".line" => {
                cur.set_pos(cur.all().len());
                true
            }
            // A target may spell `.set` again to put a mark of its own on the
            // alias; see `Architecture::alias_directive`. It comes last so
            // that no backend can take a name this table already uses.
            _ => match self.arch.alias_directive(&text) {
                Some(flags) => self.dir_set(&mut cur, span, false, flags),
                None => false,
            },
        };

        if handled {
            self.expect_end(&mut cur);
            return;
        }

        // Give the architecture a chance before reporting it unknown.
        if self.arch_directive(stmt, &text) {
            return;
        }

        if text == ".set" {
            let opt = match stmt.arg_cursor().rest().first().map(|t| t.kind) {
                Some(TokKind::Ident(n)) => self.interner.get(n).to_string(),
                _ => String::new(),
            };
            self.diags.emit(
                crate::diag::Diagnostic::error(
                    span,
                    format!(
                        "`.set {opt}` is not an option the `{}` backend understands",
                        self.arch.name()
                    ),
                )
                .with_help("to define a symbol, write `.set name, value`"),
            );
            return;
        }
        // A GNU as directive in the first column of Motorola source is a
        // label, so the word after it becomes the statement and the message
        // names that word rather than the line. GNU as `--mri` and vasm read
        // the same line the same way, so the cure is to indent it or to ask
        // for GNU syntax, and saying so beats leaving the reader to guess.
        if self.options.dialect == Dialect::Motorola
            && let Some(crate::parser::LabelDef::Named(label, _)) = stmt.labels.first()
            && let first = self.interner.get(*label).to_string()
            && first.starts_with('.')
        {
            self.diags.emit(
                crate::diag::Diagnostic::error(span, format!("unknown directive `{text}`"))
                    .with_help(format!(
                        "`{first}` in the first column is a label in Motorola syntax; \
                         indent it, or assemble with `-d gas`"
                    )),
            );
            return;
        }
        self.diags
            .error(span, format!("unknown directive `{text}`"));
    }

    /// Offers a directive to the architecture backend. Returns whether the
    /// backend claimed it.
    pub(crate) fn arch_directive(&mut self, stmt: &Statement, text: &str) -> bool {
        let args = &stmt.toks[stmt.args.min(stmt.toks.len())..];
        self.arch_directive_tokens(text, args)
    }

    /// [`Assembler::arch_directive`], for arguments that are not a statement's.
    pub(crate) fn arch_directive_tokens(
        &mut self,
        text: &str,
        args: &[crate::lexer::Token],
    ) -> bool {
        // Where a pool or padding the directive asks for is blamed.
        let span = match (args.first(), args.last()) {
            (Some(a), Some(b)) => a.span.to(b.span),
            _ => Span::DUMMY,
        };
        let mut cur = Cursor::new(args);
        let Assembler {
            arch,
            interner,
            exprs,
            diags,
            pool,
            symbols,
            arch_state,
            options,
            sections,
            cur: section,
            sm,
            implicit_it_at,
            ..
        } = self;
        let dialect = options.dialect;
        let bit_dot = arch.bit_addressing();
        let upper_modifiers = arch.uppercase_modifiers();
        let implicit_it = options.implicit_it;
        let implicit_it_here = implicit_it_at.is_some_and(|(s, _)| s == *section);
        let mut cx = crate::arch::AsmCtx {
            interner,
            exprs,
            diags,
            pool,
            symbols,
            state: arch_state,
            dialect,
            format: options.format,
            bit_dot,
            upper_modifiers,
            implicit_it,
            implicit_it_here,
            sections,
            section: *section,
            relaxable: false,
            requests: Vec::new(),
            sources: sm,
        };
        if arch.directive(&mut cx, text, &mut cur) {
            let requests = std::mem::take(&mut cx.requests);
            self.run_requests(requests, span);
            self.expect_end(&mut cur);
            return true;
        }
        false
    }

    // ---- helpers ----------------------------------------------------------

    fn push_frag(&mut self, kind: FragKind, span: Span) {
        self.cur_section().push(Fragment::new(kind, span));
    }

    /// Reads an optional `, expr` trailing byte value.
    fn optional_byte(&mut self, cur: &mut Cursor<'_>, default: u8) -> u8 {
        if cur.eat_punct(Punct::Comma).is_none() {
            return default;
        }
        match self.parse_expr(cur) {
            Some(e) => self
                .eval_absolute(e, "fill value")
                .unwrap_or(default as i64) as u8,
            None => default,
        }
    }

    fn optional_string(&mut self, cur: &mut Cursor<'_>) -> Option<String> {
        let TokKind::Str(i) = cur.peek().kind else {
            return None;
        };
        cur.advance();
        Some(String::from_utf8_lossy(self.pool.get(i)).into_owned())
    }

    pub(crate) fn expect_string(&mut self, cur: &mut Cursor<'_>, what: &str) -> Option<Vec<u8>> {
        let tok = cur.peek();
        let TokKind::Str(i) = tok.kind else {
            self.diags
                .error(tok.span, format!("expected a string {what}"));
            return None;
        };
        cur.advance();
        Some(self.pool.get(i).to_vec())
    }

    pub(crate) fn expect_name(&mut self, cur: &mut Cursor<'_>) -> Option<(Name, Span)> {
        let tok = cur.peek();
        match tok.ident() {
            Some(n) => {
                cur.advance();
                Some((n, tok.span))
            }
            None => {
                self.diags.error(tok.span, "expected a symbol name");
                None
            }
        }
    }

    // ---- data -------------------------------------------------------------

    /// A data directive of `size`-byte values. `aligned` marks the ones that
    /// start on their own boundary where the target asks for that.
    fn dir_data(
        &mut self,
        cur: &mut Cursor<'_>,
        size: u8,
        span: Span,
        aligned: bool,
        directive: &str,
    ) -> bool {
        if cur.at_end() {
            return true;
        }
        self.map_data();
        if aligned && self.arch.aligns_data() {
            self.align_data(size as u64, span);
        }
        loop {
            let mark = self.exprs.len();
            let Some(e) = self.parse_data_expr(cur, directive) else {
                return true;
            };
            if self.dwarf.line.pending {
                self.dwarf_data();
            }
            // `.` in each value is the address of that value, not of the
            // statement: `.long a - ., b - .` is two PC-relative values in
            // GNU as and llvm-mc alike.
            if self.here_sym.is_some() {
                self.here_sym = Some(self.anon_label(span));
                self.bind_positional(mark);
            }
            self.emit_value_in(size, e, span, directive);
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    /// One value of a data directive. On a target whose relocation modifiers
    /// are written as a call around the whole value (AVR's `.word pm(main)`;
    /// see [`Architecture::expr_modifiers`]) that call is read here, as GNU
    /// as reads it in `avr_parse_cons_expression`: only directly after the
    /// directive or a comma, and only where a `(` follows the name.
    ///
    /// The suffix spelling, ARM's `.word sym(GOT)`, is the expression
    /// parser's, since it binds to one symbol inside a larger expression;
    /// see [`Architecture::data_paren_modifiers`].
    ///
    /// [`Architecture::expr_modifiers`]: crate::arch::Architecture::expr_modifiers
    /// [`Architecture::data_paren_modifiers`]: crate::arch::Architecture::data_paren_modifiers
    ///
    /// AArch64's `%dtprel(sym)` is read here too, in the directives its
    /// GNU as reads it in; see [`Architecture::percent_modifiers`].
    ///
    /// [`Architecture::percent_modifiers`]: crate::arch::Architecture::percent_modifiers
    fn parse_data_expr(&mut self, cur: &mut Cursor<'_>, directive: &str) -> Option<ExprRef> {
        let tok = cur.peek();
        if tok.is_punct(Punct::Percent)
            && cur.nth(2).is_punct(Punct::LParen)
            && let Some(name) = cur.nth(1).ident().and_then(|n| {
                let text = self.interner.get(n);
                self.arch
                    .percent_modifiers(directive)
                    .iter()
                    .find(|m| text == **m)
                    .copied()
            })
        {
            cur.advance();
            cur.advance();
            return self.modifier_call(cur, name, tok.span);
        }
        let name = tok.ident().and_then(|n| {
            let text = self.interner.get(n);
            self.arch
                .expr_modifiers()
                .iter()
                .find(|m| text.eq_ignore_ascii_case(m))
                .copied()
        });
        let Some(name) = name.filter(|_| cur.nth(1).is_punct(Punct::LParen)) else {
            return self.parse_expr_with_suffixes(cur, self.arch.data_paren_modifiers());
        };
        cur.advance();
        self.modifier_call(cur, name, tok.span)
    }

    /// The `(expr)` of a modifier written as a call, `pm(main)` or
    /// `%dtprel(sym)`, from its `(` on; `start` is where the call began.
    fn modifier_call(&mut self, cur: &mut Cursor<'_>, name: &str, start: Span) -> Option<ExprRef> {
        let open = cur.advance();
        let inner = self.parse_expr(cur)?;
        let Some(close) = cur.eat_punct(Punct::RParen) else {
            self.diags.emit(
                crate::diag::Diagnostic::error(cur.peek().span, "expected `)`")
                    .with_note(open.span, "to match this `(`"),
            );
            return None;
        };
        let name = self.interner.intern(name);
        let kind = crate::expr::ExprKind::Modifier(name, inner);
        Some(self.exprs.alloc(kind, start.to(close.span)))
    }

    /// Pads to a `size`-byte boundary ahead of data that must start on one,
    /// and remembers the padding so that layout can refuse any it needed; see
    /// [`Architecture::aligns_data`](crate::arch::Architecture::aligns_data).
    fn align_data(&mut self, size: u64, span: Span) {
        let frag = self.cur_section().push(Fragment::new(
            FragKind::Align {
                align: size,
                fill: vec![0],
                max_skip: None,
                pad: 0,
                nop_state: None,
            },
            span,
        ));
        self.align_tests.push((self.cur, frag));
        self.section_mut(self.cur).align = self.section(self.cur).align.max(size);
    }

    /// Emits `size` bytes for `e`, as literal bytes when it already folds to a
    /// constant and as a fixup otherwise.
    pub(crate) fn emit_value(&mut self, size: u8, e: ExprRef, span: Span) {
        self.emit_value_of(size, e, span, None);
    }

    /// Emits `size` bytes for `e` on behalf of the data directive spelled
    /// `directive`, which decides whether a relocation modifier is read there
    /// at all; see [`Architecture::directive_modifiers`].
    ///
    /// [`Architecture::directive_modifiers`]:
    ///     crate::arch::Architecture::directive_modifiers
    pub(crate) fn emit_value_in(&mut self, size: u8, e: ExprRef, span: Span, directive: &str) {
        self.emit_value_of(size, e, span, Some(directive));
    }

    /// The body of both, with `None` for a value no data directive read: an
    /// ARM literal pool's entry, whose modifier the instruction that asked
    /// for the pool has already had read for it.
    fn emit_value_of(&mut self, size: u8, e: ExprRef, span: Span, directive: Option<&str>) {
        // A zero takes space in a section with no contents, in GNU as and
        // llvm-mc alike: Clang writes a zero-initialised AVR global as
        // `.short 0` in `.bss`. Anything else has nowhere to go.
        if self.section(self.cur).kind == SectionKind::Nobits
            && self.eval_ref(e).ok().and_then(|v| v.as_abs()) == Some(0)
        {
            let size = self.exprs.int(size as u64, span);
            let fill = self.exprs.int(0, span);
            self.push_frag(
                FragKind::Space {
                    size,
                    fill,
                    resolved: 0,
                },
                span,
            );
            return;
        }
        if self.check_nobits(span) {
            return;
        }
        self.bind_here_to_item(e);
        let mut reloc = self.arch.data_reloc(size, false).unwrap_or(0);
        let mut kind = crate::section::FixupKind::data(size);
        // A modifier the target does not recognise used to fall back to the
        // plain data relocation, so `.long foo@got` quietly became an
        // absolute reference to `foo`. That is a different program, so it is
        // an error instead. The check comes before the constant fold below
        // because a modifier can be wrong for the field's width whatever the
        // value is: AVR has `pm()` for a `.word` and none for a `.byte`.
        // A Mach-O object checks its modifiers against its own
        // relocations, once it builds them.
        if let Some(name) = self.modifier_chain(e)
            && !self.macho_object()
        {
            // COFF's own modifiers (`@IMGREL`) are the format's, not the
            // backend's: they name what the field holds, not a relocation
            // number; see `crate::coff::modifier_class`.
            let coff = self
                .options
                .format
                .is_coff()
                .then(|| crate::coff::modifier_class(&name))
                .flatten();
            if let Some(class) = coff {
                let kind = crate::section::FixupKind::data(size)
                    .with_reloc(reloc)
                    .with_class(class);
                let espan = self.exprs.span(e);
                self.cur_section().emit_fixup(size, e, kind, espan);
                return;
            }
            // A backend that reads its modifiers in some data directives and
            // not others answers for the one that read this value; see
            // `Architecture::directive_modifiers`.
            if let Some(dirs) = self.arch.directive_modifiers()
                && let Some(directive) = directive
                && !dirs.contains(&directive)
            {
                let espan = self.exprs.span(e);
                let written = crate::arch::written_modifier(&*self.arch, &name);
                self.diags.error(
                    espan,
                    format!(
                        "the `{}` backend reads no relocation modifier in `{directive}`, so \
                         {written} is not one here",
                        self.arch.name()
                    ),
                );
                return;
            }
            match self.arch.modifier_reloc(&name, size, false) {
                Some(r) => {
                    reloc = r;
                    // A thread-local model names a variable, whose offset
                    // only the linker lays out; a number has none. llvm-mc
                    // refuses one, and GNU as refuses it or, for PowerPC,
                    // crashes.
                    if self.options.dialect != crate::lexer::Dialect::Nasm
                        && self.arch.modifier_symbols(&name).tls
                        && self.eval_ref(e).is_ok_and(|v| v.is_absolute())
                    {
                        let espan = self.exprs.span(e);
                        self.diags.error(
                            espan,
                            format!(
                                "`@{name}` names a thread-local variable, so it needs a symbol \
                                 rather than a number"
                            ),
                        );
                        return;
                    }
                    // One that takes part of the value writes the field
                    // itself; the value it takes that part of is what has
                    // to fit.
                    if let crate::arch::FlatModifier::Field { write, unit } =
                        self.arch.flat_modifier(&name)
                    {
                        kind = kind.with_field(63, unit).scatter(write);
                    }
                }
                None => {
                    let espan = self.exprs.span(e);
                    let written = crate::arch::written_modifier(&*self.arch, &name);
                    self.diags.error(
                        espan,
                        format!(
                            "{written} is not a relocation modifier the `{}` backend supports \
                             in a {size}-byte data field",
                            self.arch.name()
                        ),
                    );
                    return;
                }
            }
        }
        // Resolve now if it already has a value: a `.set` symbol is a
        // snapshot at each use, so a later redefinition must not reach back
        // and change bytes that were already emitted.
        // In a Mach-O object a difference of labels already a fixed distance
        // apart is a value too; see `Assembler::macho_fixed_difference`.
        // A size is something the file states about a symbol, so a value the
        // expression works out to on its own is not one to read a size from:
        // GNU as calls `5@SIZE`, and `n@SIZE` after `.set n, 7`, invalid
        // expressions. Leaving them to the fixup, which has no symbol to
        // name, is what refuses them.
        if let Some(v) = self
            .eval_ref(e)
            .ok()
            .and_then(|v| v.as_abs())
            .or_else(|| self.macho_fixed_difference(e))
            .filter(|_| !self.names_size_modifier(e))
        {
            if !kind.fits(v as i128) {
                let espan = self.exprs.span(e);
                let msg = if kind.value_align > 1 && v % kind.value_align as i64 != 0 {
                    format!("value {v} is not a multiple of {}", kind.value_align)
                } else {
                    format!("value {v} does not fit in {size} byte(s)")
                };
                self.diags.error(espan, msg);
                return;
            }
            let mut bytes = vec![0; size as usize];
            kind.write(self.arch.endian(), &mut bytes, v);
            self.cur_section().emit_bytes(&bytes, span);
            return;
        }
        let kind = kind.with_reloc(reloc);
        let espan = self.exprs.span(e);
        self.cur_section().emit_fixup(size, e, kind, espan);
    }

    fn dir_ascii(&mut self, cur: &mut Cursor<'_>, terminate: bool, span: Span) -> bool {
        if cur.at_end() {
            return true;
        }
        self.map_data();
        loop {
            let Some(mut bytes) = self.expect_string(cur, "literal") else {
                return true;
            };
            if terminate {
                bytes.push(0);
            }
            if self.dwarf.line.pending {
                self.dwarf_data();
            }
            self.emit_bytes(&bytes, span);
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_leb(&mut self, cur: &mut Cursor<'_>, signed: bool, span: Span) -> bool {
        self.map_data();
        loop {
            let Some(e) = self.parse_expr(cur) else {
                return true;
            };
            // llvm-mc writes a constant as bytes, which consume a `.loc`.
            if self.dwarf.line.pending && self.eval_ref(e).is_ok_and(|v| v.is_absolute()) {
                self.dwarf_data();
            }
            if !self.check_nobits(span) {
                self.push_frag(
                    FragKind::Leb128 {
                        value: e,
                        signed,
                        encoded: vec![0],
                    },
                    span,
                );
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_space(&mut self, cur: &mut Cursor<'_>, span: Span, zero_only: bool) -> bool {
        let Some(size) = self.parse_expr(cur) else {
            return true;
        };
        let fill = if zero_only {
            self.exprs.int(0, span)
        } else if cur.eat_punct(Punct::Comma).is_some() {
            match self.parse_expr(cur) {
                Some(e) => e,
                None => return true,
            }
        } else {
            self.exprs.int(0, span)
        };
        // A `.space` of a known size is a fill fragment to GNU as, which
        // marks its own start; one it cannot size yet is not.
        self.map_data();
        if self
            .eval_ref(size)
            .ok()
            .and_then(|v| v.as_abs())
            .is_some_and(|n| n > 0)
        {
            self.map_data_frag();
        }
        self.push_frag(
            FragKind::Space {
                size,
                fill,
                resolved: 0,
            },
            span,
        );
        true
    }

    fn dir_fill(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(count_e) = self.parse_expr(cur) else {
            return true;
        };
        let Some(count) = self.eval_absolute(count_e, "`.fill` count") else {
            return true;
        };
        let mut size: i64 = 1;
        let mut value: i64 = 0;
        if cur.eat_punct(Punct::Comma).is_some() {
            let Some(e) = self.parse_expr(cur) else {
                return true;
            };
            size = self.eval_absolute(e, "`.fill` size").unwrap_or(1);
            if cur.eat_punct(Punct::Comma).is_some() {
                let Some(e) = self.parse_expr(cur) else {
                    return true;
                };
                value = self.eval_absolute(e, "`.fill` value").unwrap_or(0);
            }
        }
        if count < 0 || size < 0 {
            self.diags
                .error(span, "`.fill` count and size must not be negative");
            return true;
        }
        if !(0..=8).contains(&size) {
            self.diags
                .error(span, "`.fill` size must be between 0 and 8");
            return true;
        }
        let total = (count as u64).saturating_mul(size as u64);
        if total > (1 << 28) {
            self.diags
                .error(span, "`.fill` would emit more than 256 MiB");
            return true;
        }
        self.map_data();
        if total > 0 {
            self.map_data_frag();
        }
        // llvm-mc writes `.fill` as values, which consume a `.loc`.
        if self.dwarf.line.pending {
            self.dwarf_data();
        }
        let unit = self.arch.endian().bytes(value as u64, size as usize);
        let mut bytes = Vec::with_capacity(total as usize);
        for _ in 0..count {
            bytes.extend_from_slice(&unit);
        }
        self.emit_bytes(&bytes, span);
        true
    }

    fn dir_incbin(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(name) = self.expect_string(cur, "file name") else {
            return true;
        };
        let name = String::from_utf8_lossy(&name).into_owned();
        let Some(path) = self.find_include(&name) else {
            self.diags.error(span, format!("cannot find `{name}`"));
            return true;
        };
        match std::fs::read(&path) {
            Ok(data) => {
                self.map_data();
                if self.dwarf.line.pending {
                    self.dwarf_data();
                }
                self.emit_bytes(&data, span)
            }
            Err(e) => self
                .diags
                .error(span, format!("cannot read `{}`: {e}", path.display())),
        }
        true
    }

    // ---- layout -----------------------------------------------------------

    pub(crate) fn dir_align(
        &mut self,
        cur: &mut Cursor<'_>,
        span: Span,
        power_of_two: bool,
    ) -> bool {
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let Some(n) = self.eval_absolute(e, "alignment") else {
            return true;
        };
        if n < 0 || n > 60 && power_of_two {
            self.diags.error(span, "alignment is out of range");
            return true;
        }
        let align: u64 = if power_of_two {
            1u64 << n
        } else {
            if n == 0 {
                return true;
            }
            if !(n as u64).is_power_of_two() {
                self.diags
                    .error(span, format!("alignment {n} is not a power of two"));
                return true;
            }
            n as u64
        };

        let mut fill_expr = None;
        let mut max_skip = None;
        if cur.eat_punct(Punct::Comma).is_some() {
            if !cur.check_punct(Punct::Comma) {
                fill_expr = self.parse_expr(cur);
            }
            if cur.eat_punct(Punct::Comma).is_some()
                && let Some(e) = self.parse_expr(cur)
            {
                max_skip = self
                    .eval_absolute(e, "`.align` maximum skip")
                    .map(|v| v.max(0) as u64);
            }
        }

        // Executable sections pad with real no-ops so the padding stays
        // executable; everything else pads with zeroes.
        let fill = match fill_expr {
            Some(e) => vec![self.eval_absolute(e, "fill value").unwrap_or(0) as u8],
            None if self.section(self.cur).flags.exec => Vec::new(),
            None => vec![0],
        };
        // GNU as makes no fragment for an alignment of one byte, and marks
        // the start of any other: as code where no-ops pad it.
        // No-ops are for the instruction set of the last instruction, if one
        // has been assembled since the last fragment ended; see
        // `Section::nop_state`.
        let nop_state = self
            .section(self.cur)
            .nop_state
            .clone()
            .unwrap_or_else(|| self.arch_state.clone());
        if align > 1 {
            if fill.is_empty() {
                self.map_code_align(&nop_state);
            } else {
                self.map_data_frag();
            }
        }
        // A target whose linker deletes instructions cannot have the
        // alignment settled here; see `Architecture::align_reloc`. The state
        // that decides it is the one in force at the directive, as it is in
        // GNU as, which reads `.option norvc` and `.option norelax` the same
        // way for it. A flat image has no linker to hand it to, and an object
        // for another machine no relocation to carry it, so both settle the
        // alignment here as every other target does. The maximum skip goes
        // with it: GNU as passes it to the ordinary alignment and not to
        // this one, since only the linker knows how far it will have to
        // skip.
        if fill.is_empty()
            && self.options.relocatable
            && self.arch.elf_machine() == self.target().elf_machine()
            && let Some((unit, kind)) = self.arch.align_reloc(&self.arch_state, align)
        {
            self.push_frag(
                FragKind::Align {
                    align: unit,
                    nop_state: Some(nop_state.clone()),
                    fill,
                    max_skip: None,
                    pad: 0,
                },
                span,
            );
            let pad = align - unit;
            let value = self.exprs.int(pad, span);
            let nops = self.arch.nop_fill(&nop_state, pad);
            let s = self.cur_section();
            s.emit_variants(
                vec![crate::section::Variant {
                    bytes: nops,
                    fixups: vec![crate::section::Fixup {
                        offset: 0,
                        expr: value,
                        kind,
                        span,
                    }],
                }],
                span,
            );
            self.section_mut(self.cur).align = self.section(self.cur).align.max(align);
            return true;
        }
        self.push_frag(
            FragKind::Align {
                align,
                nop_state: fill.is_empty().then_some(nop_state),
                fill,
                max_skip,
                pad: 0,
            },
            span,
        );
        self.section_mut(self.cur).align = self.section(self.cur).align.max(align);
        true
    }

    // ---- sections ---------------------------------------------------------

    fn dir_section(&mut self, cur: &mut Cursor<'_>, span: Span, push: bool) -> bool {
        let tok = cur.peek();
        let name = match tok.kind {
            // A name is everything up to a comma or a space, as GNU as reads
            // it: `.note.GNU-stack` is one name, not a subtraction.
            TokKind::Ident(n) => {
                let first = cur.advance();
                let mut last = first;
                while !cur.peek().is_eol()
                    && !cur.peek().is_punct(Punct::Comma)
                    && !cur.peek().preceded_by_space
                {
                    last = cur.advance();
                }
                if last.span == first.span {
                    n
                } else {
                    let text = self.sm.span_text(first.span.to(last.span)).to_string();
                    self.interner.intern(&text)
                }
            }
            TokKind::Str(i) => {
                cur.advance();
                let s = String::from_utf8_lossy(self.pool.get(i)).into_owned();
                self.interner.intern(&s)
            }
            _ => {
                self.diags.error(tok.span, "expected a section name");
                return true;
            }
        };

        let text = self.interner.get(name).to_string();
        // GNU as for MSP430 refers to the C runtime's set-up routine for the
        // section as soon as it is named; see `Architecture::section_symbols`.
        for sym in self.target().section_symbols(&text) {
            self.refer_to_symbol(sym, tok.span);
        }
        let builtin = builtin_section_kind(&text);
        let (mut kind, mut flags) =
            builtin.unwrap_or((SectionKind::Progbits, SectionFlags::default()));
        // `SHF_TLS` is ELF's alone, so a COFF or Mach-O section that happens
        // to be called `.tdata` is an ordinary one there.
        flags.tls &= self.options.format == crate::output::Format::Elf;
        let mut entsize = 0u64;

        // A COFF section's attributes are its own: flag letters that mean
        // different things, and a COMDAT selection where ELF has a type.
        if self.options.format.is_coff() {
            let mut info = None;
            let mut key = name;
            if cur.eat_punct(Punct::Comma).is_some() {
                let (characteristics, comdat, k) =
                    crate::coff::parse_section_attributes(self, &mut *cur, &text, span);
                flags = crate::coff::section_flags(characteristics);
                kind = k;
                info = Some(crate::coff::SectionInfo {
                    characteristics,
                    comdat,
                });
                // llvm-mc tells sections apart by name and COMDAT symbol.
                if let Some(sym) = comdat.and_then(|c| c.symbol) {
                    let sym = self.interner.get(self.symbols.get(sym).name).to_string();
                    key = self.interner.intern(&format!("{text}\u{0}{sym}"));
                }
            }
            let id = self.get_or_create_section(key, kind, flags, 1);
            // The first description of a section is the one that counts.
            if let Some(info) = info {
                self.coff.sections.entry(id).or_insert(info);
            }
            if push {
                self.push_section_stack();
            }
            self.set_section(id);
            return true;
        }

        // GNU as opens `.text`, `.data` and `.bss` before it reads a line, so
        // `.section` naming one of them describes a section that already
        // exists, and the description is ignored as it is for any section
        // named twice: `.section .data,"awT"` leaves `.data` the ordinary
        // data section rather than making it thread-local.
        let standing = matches!(text.as_str(), ".text" | ".data" | ".bss");
        if cur.eat_punct(Punct::Comma).is_some() {
            if let Some(s) = self.expect_string(cur, "of section flags") {
                let written = parse_flags(&String::from_utf8_lossy(&s));
                flags = match builtin {
                    Some((_, b)) if !standing => builtin_section_flags(b, written),
                    Some(_) => flags,
                    None => written,
                };
                flags.tls &= self.options.format == crate::output::Format::Elf;
            }
            // `,@progbits` or `,%progbits`
            if cur.eat_punct(Punct::Comma).is_some() {
                if cur.eat_punct(Punct::At).is_none() {
                    cur.eat_punct(Punct::Percent);
                }
                // A type may also be written as its number, as llvm-mc writes
                // `@0x7000001e` for MIPS's DWARF sections; any such section
                // holds bits.
                if let TokKind::Int(_) = cur.peek().kind {
                    cur.advance();
                } else if let Some((tn, _)) = self.expect_name(cur)
                    && !standing
                {
                    let t = self.interner.get(tn).to_ascii_lowercase();
                    kind = match t.as_str() {
                        "nobits" => SectionKind::Nobits,
                        "note" => SectionKind::Note,
                        _ => SectionKind::Progbits,
                    };
                }
                if cur.eat_punct(Punct::Comma).is_some()
                    && let Some(e) = self.parse_expr(cur)
                    && !standing
                {
                    entsize = self
                        .eval_absolute(e, "section entry size")
                        .unwrap_or(0)
                        .max(0) as u64;
                }
            }
        }

        let id = self.get_or_create_section(name, kind, flags, 1);
        self.section_mut(id).entsize = entsize;
        if push {
            self.push_section_stack();
        }
        self.set_section(id);
        if self.dwarf.line.source.on {
            self.dwarf_section_named(id);
        }
        true
    }

    // ---- symbols ----------------------------------------------------------

    fn dir_binding(&mut self, cur: &mut Cursor<'_>, binding: Binding, _span: Span) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let id = self.symbols.intern(name, span);
            let elf = self.options.format == crate::output::Format::Elf;
            let sym = self.symbols.get_mut(id);
            // GNU as keeps a symbol weak once `.weak` has named it, whatever
            // `.globl` says after, where llvm-mc refuses the pair; and a
            // `.local` after `.comm` leaves the block common and global.
            let keep = elf
                && match binding {
                    Binding::Global => sym.binding == Binding::Weak,
                    Binding::Local => matches!(sym.value, SymbolValue::Common { .. }),
                    _ => false,
                };
            if !keep {
                sym.binding = binding;
            }
            sym.declared = true;
            if binding == Binding::Local && !sym.is_defined() {
                sym.declared_local = true;
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_visibility(&mut self, cur: &mut Cursor<'_>, vis: Visibility, _span: Span) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let id = self.symbols.intern(name, span);
            let sym = self.symbols.get_mut(id);
            sym.visibility = vis;
            sym.declared = true;
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    /// True for `.set word` — a single identifier and nothing else, which no
    /// assignment can be. Returning `false` from the table sends it on to the
    /// architecture's directive hook.
    /// Whether a `.file` is the numbered form, which is DWARF's whatever the
    /// object format; `.file "name"` alone is the source file's name.
    fn is_numbered_file(cur: &Cursor<'_>) -> bool {
        matches!(
            cur.peek().kind,
            TokKind::Int(_) | TokKind::Punct(Punct::Minus)
        )
    }

    fn is_set_option(cur: &Cursor<'_>) -> bool {
        let rest = cur.rest();
        rest.len() == 1 && matches!(rest[0].kind, TokKind::Ident(_))
    }

    /// `.set name, value`, and the spellings that mean the same.
    ///
    /// `flags` are the target flags the directive puts on the alias on top of
    /// the ones its value carries; see [`Architecture::alias_directive`].
    ///
    /// [`Architecture::alias_directive`]: crate::arch::Architecture::alias_directive
    pub(crate) fn dir_set(
        &mut self,
        cur: &mut Cursor<'_>,
        span: Span,
        once_only: bool,
        flags: u8,
    ) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` after the symbol name");
            return true;
        }
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let id = self.symbols.intern(name, nspan);
        if once_only && self.symbols.get(id).is_defined() {
            let prev = self.symbols.get(id).def_span;
            let display = self.display_name(id);
            self.diags.emit(
                crate::diag::Diagnostic::error(span, format!("`{display}` is already defined"))
                    .with_note(prev, "previous definition is here"),
            );
            return true;
        }
        // GNU as copies the symbol's size onto an alias of it, so a `.size`
        // given before the `.set` is the alias's size too and `@SIZE` of the
        // alias reads it. What is copied is a snapshot, taken whatever offset
        // the alias adds: a `.size` on the source after this point does not
        // reach the alias. A source still undefined here has no size to
        // snapshot, and there GNU as leaves the alias pointing at it, so the
        // alias ends up with whatever the source is given later -- but only
        // where the alias adds nothing, since an offset is what makes GNU as
        // settle the equate into a number instead.
        let head = match self
            .head_symbol(e)
            .map(|h| (self.exprs.get(h).kind.clone(), h))
        {
            Some((crate::expr::ExprKind::Sym(n), h)) => {
                let hspan = self.exprs.span(h);
                Some(self.symbols.intern(n, hspan))
            }
            Some((crate::expr::ExprKind::SymId(s), _)) => Some(s),
            _ => None,
        };
        // A symbol plus a number also carries the source's target flags onto
        // the alias: that is GNU as's `copy_symbol_attributes`, which
        // `pseudo_set` calls for a value that is `O_symbol` and which on ARM
        // assigns the Thumb marks, so an alias of a Thumb function is one
        // too. A difference of labels or a number is not `O_symbol` and
        // carries nothing. The source is the symbol as written, not what it
        // may itself be an alias of, since that is the symbol GNU as's
        // expression holds.
        let mut inherited = 0;
        if let Some(src) = head.filter(|s| *s != id) {
            let from = self.symbols.get(src);
            let (size, defined) = (from.size, from.is_defined());
            inherited = from.target_flags;
            let plain = self
                .eval_ref(e)
                .is_ok_and(|v| v.plus == Some(src) && v.minus.is_none() && v.addend == 0);
            let sym = self.symbols.get_mut(id);
            sym.size = if defined { size } else { None };
            sym.size_from = (!defined && plain).then_some(src);
        }
        let sym = self.symbols.get_mut(id);
        sym.target_flags = inherited | flags;
        sym.value = SymbolValue::Expr(e);
        sym.def_span = nspan;
        sym.redefinable = !once_only;
        self.symbols.mark_defined(id);
        true
    }

    fn dir_size(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` after the symbol name");
            return true;
        }
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let id = self.symbols.intern(name, nspan);
        let sym = self.symbols.get_mut(id);
        sym.size = Some(e);
        sym.declared = true;
        true
    }

    fn dir_type(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` after the symbol name");
            return true;
        }
        // The type is written `@function`, `%function`, `#function` or
        // `STT_FUNC`. Which sigil a target's source uses is whichever one is
        // not already taken: `@` starts a comment on ARM, `%` names a
        // register on SPARC and PowerPC, so SPARC sources write `#function`.
        if cur.eat_punct(Punct::At).is_none() && cur.eat_punct(Punct::Percent).is_none() {
            cur.eat_punct(Punct::Hash);
        }
        let Some((tname, tspan)) = self.expect_name(cur) else {
            return true;
        };
        // Where `@` may start a name (COFF), `@function` is one word.
        let t = self.interner.get(tname).to_ascii_lowercase();
        let ty = match t.trim_start_matches('@').trim_start_matches("stt_") {
            "function" | "func" => SymType::Func,
            "object" => SymType::Object,
            "notype" => SymType::NoType,
            "tls_object" | "tls" => SymType::Tls,
            "common" => SymType::Object,
            other => {
                self.diags
                    .error(tspan, format!("unknown symbol type `{other}`"));
                return true;
            }
        };
        let id = self.symbols.intern(name, nspan);
        let sym = self.symbols.get_mut(id);
        sym.ty = ty;
        sym.declared = true;
        let _ = span;
        true
    }

    /// `.comm`, `.lcomm` and `.tls_common`, which differ only in the binding
    /// and the type the block gets: a thread-local common block is
    /// `STT_TLS`, and the linker gathers it into the thread-local image
    /// rather than into `.bss`.
    fn dir_comm(&mut self, cur: &mut Cursor<'_>, span: Span, lcomm: bool, ty: SymType) -> bool {
        let Some((name, nspan)) = self.expect_name(cur) else {
            return true;
        };
        if cur.eat_punct(Punct::Comma).is_none() {
            self.diags.error(span, "expected `,` and a size");
            return true;
        }
        let Some(e) = self.parse_expr(cur) else {
            return true;
        };
        let Some(size) = self.eval_absolute(e, "`.comm` size") else {
            return true;
        };
        let mut given = None;
        let mut align_span = span;
        if cur.eat_punct(Punct::Comma).is_some() {
            align_span = cur.peek().span;
            if let Some(e) = self.parse_expr(cur) {
                let v = self.eval_absolute(e, "`.comm` alignment").unwrap_or(1);
                given = Some(v.max(0) as u64);
            }
        }
        if size < 0 {
            self.diags.error(span, "`.comm` size must not be negative");
            return true;
        }
        let size = size as u64;
        let coff = self.options.format.is_coff();
        let elf = self.options.format == crate::output::Format::Elf;
        // Neither format has a local common block. `.lcomm` reserves the
        // object in `.bss` instead, and on ELF so does a `.comm` of a symbol
        // `.local` named first, which is where llvm-mc and GNU as put both.
        let declared_local = self
            .symbols
            .lookup(name)
            .is_some_and(|id| self.symbols.get(id).declared_local);
        if lcomm || (elf && declared_local) {
            let rule = self.target().local_common();
            let (align, sym_ty) = if lcomm {
                if given.is_some() && !coff && !rule.takes_align {
                    self.diags
                        .error(align_span, "`.lcomm` takes no alignment on this target");
                    return true;
                }
                // COFF objects follow llvm-mc, which packs `.bss`; the mingw
                // GNU as aligns by size there too.
                let implicit = if coff { 1 } else { (rule.align)(size) };
                (given.unwrap_or(implicit), rule.ty)
            } else {
                // GNU as reads an alignment of 0 as none here too.
                (given.filter(|&a| a != 0).unwrap_or(1), SymType::Object)
            };
            if !align.is_power_of_two() {
                self.diags
                    .error(align_span, "the alignment must be a power of two");
                return true;
            }
            // COFF has no symbol types to give; a thread-local block keeps
            // its type in `.bss`, as GNU as leaves it there.
            let ty = if coff {
                None
            } else if ty == SymType::Tls {
                Some(ty)
            } else {
                Some(sym_ty)
            };
            self.reserve_bss(name, nspan, size, align, ty, span);
            for sym in self.target().common_symbols() {
                self.refer_to_symbol(sym, nspan);
            }
            return true;
        }
        let mut size = size;
        // A COFF common block records only its size, and `.comm`'s alignment
        // is a power of two there, as both references read it. llvm-mc makes
        // the block at least that large, which is how the linker, placing it
        // at a boundary of its size, honours the alignment.
        let align = if coff {
            match given {
                Some(log2) if log2 > 5 => {
                    self.diags.error(
                        span,
                        format!(
                            "a COFF common block can be aligned to at most 32 bytes, not 2^{log2}"
                        ),
                    );
                    return true;
                }
                Some(log2) => {
                    size = size.max(1 << log2);
                    1 << log2
                }
                None => 1,
            }
        } else {
            match given {
                Some(align) if align != 0 => align,
                // GNU as, for every ELF target, aligns a block that names no
                // alignment, or 0, to its size rounded up to a power of two,
                // but to no more than 16 bytes. llvm-mc aligns it to 1.
                _ => size.max(1).next_power_of_two().min(16),
            }
        };
        let id = self.symbols.intern(name, nspan);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Common { size, align };
        sym.def_span = nspan;
        sym.ty = ty;
        sym.binding = Binding::Global;
        for sym in self.target().common_symbols() {
            self.refer_to_symbol(sym, nspan);
        }
        true
    }

    /// Reserves `size` zero bytes in `.bss` for `name`, aligned to `align`,
    /// and leaves the current section as it was: what `.lcomm` does, and on
    /// ELF a `.comm` of a symbol declared `.local`.
    fn reserve_bss(
        &mut self,
        name: crate::intern::Name,
        nspan: Span,
        size: u64,
        align: u64,
        ty: Option<SymType>,
        span: Span,
    ) {
        let bss = self.standard_section(".bss");
        let saved = self.cur;
        self.set_section(bss);
        // GNU as pads to the alignment with a fragment of zeros, which on ARM
        // and AArch64 is data the first time; the object itself it reserves
        // without marking anything.
        if align > 1 {
            self.map_data_frag();
        }
        self.align_to(align, span);
        let size = self.exprs.int(size, span);
        // Both references give the symbol the object's size and type, as if
        // `.size` and `.type` had.
        if let Some(id) = self.define_label(&crate::parser::LabelDef::Named(name, nspan))
            && let Some(ty) = ty
        {
            let sym = self.symbols.get_mut(id);
            sym.ty = ty;
            sym.size = Some(size);
        }
        let fill = self.exprs.int(0, span);
        self.cur_section().push(crate::section::Fragment::new(
            crate::section::FragKind::Space {
                size,
                fill,
                resolved: 0,
            },
            span,
        ));
        self.set_section(saved);
    }

    // ---- conditionals -----------------------------------------------------

    fn dir_if(&mut self, cur: &mut Cursor<'_>, kind: &str, span: Span) -> bool {
        // A conditional nested inside a false branch is pushed inactive
        // without evaluating its condition, which may not even be resolvable.
        if !self.cond_active() {
            self.push_cond(Cond {
                active: false,
                taken: true,
                seen_else: false,
                span,
            });
            cur.set_pos(cur.all().len());
            return true;
        }
        let value = self.eval_condition(cur, kind);
        self.push_cond(Cond {
            active: value,
            taken: value,
            seen_else: false,
            span,
        });
        true
    }

    fn eval_condition(&mut self, cur: &mut Cursor<'_>, kind: &str) -> bool {
        match kind {
            ".ifdef" | ".ifndef" => {
                let defined = match self.expect_name(cur) {
                    Some((name, _)) => self
                        .symbols
                        .lookup(name)
                        .is_some_and(|id| self.symbols.get(id).is_defined()),
                    None => false,
                };
                if kind == ".ifdef" { defined } else { !defined }
            }
            ".ifb" | ".ifnb" => {
                let blank = cur.at_end() || cur.is_empty();
                cur.set_pos(cur.all().len());
                if kind == ".ifb" { blank } else { !blank }
            }
            _ => {
                let mark = self.exprs.len();
                let Some(e) = self.parse_expr(cur) else {
                    return false;
                };
                // CC-RX takes a symbol not defined yet as 0 here
                // (R20UT3248EJ0115 page 495).
                if self.options.dialect == Dialect::CcRx {
                    self.ccrx_undefined_as_zero(mark);
                }
                let Some(v) = self.eval_absolute(e, "`.if` condition") else {
                    return false;
                };
                match kind {
                    ".ifeq" => v == 0,
                    ".ifne" => v != 0,
                    _ => v != 0,
                }
            }
        }
    }

    fn dir_elseif(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        self.dir_elseif_kind(cur, span, ".if")
    }

    /// `.elseif`, with the test of the `.if` variant `kind`: CC-RL's
    /// `$ELSEIFN` is an `.elseif` that tests like `.ifeq`.
    pub(crate) fn dir_elseif_kind(&mut self, cur: &mut Cursor<'_>, span: Span, kind: &str) -> bool {
        let Some(state) = self.cond_top() else {
            self.diags.error(span, "`.elseif` without a matching `.if`");
            cur.set_pos(cur.all().len());
            return true;
        };
        if state.seen_else {
            self.diags.error(span, "`.elseif` after `.else`");
            cur.set_pos(cur.all().len());
            return true;
        }
        if state.taken {
            // An earlier branch already won; skip this condition entirely
            // rather than evaluating something that may not resolve.
            self.set_cond_active(false);
            cur.set_pos(cur.all().len());
            return true;
        }
        // Evaluating the condition needs the enclosing conditional to look
        // active, which it is: `taken` is false only when no branch ran.
        let outer_active = self.enclosing_cond_active();
        let value = outer_active && self.eval_condition(cur, kind);
        self.set_cond_active(value);
        if value {
            self.mark_cond_taken();
        }
        true
    }

    fn dir_else(&mut self, span: Span) {
        let Some(state) = self.cond_top() else {
            self.diags.error(span, "`.else` without a matching `.if`");
            return;
        };
        if state.seen_else {
            self.diags.error(span, "duplicate `.else`");
            return;
        }
        let taken = state.taken;
        let outer_active = self.enclosing_cond_active();
        self.mark_cond_else();
        self.set_cond_active(outer_active && !taken);
        if !taken {
            self.mark_cond_taken();
        }
    }

    // ---- files ------------------------------------------------------------

    pub(crate) fn find_include(&self, name: &str) -> Option<PathBuf> {
        let direct = PathBuf::from(name);
        if direct.is_absolute() && direct.exists() {
            return Some(direct);
        }
        for dir in &self.options.include_paths {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
        direct.exists().then_some(direct)
    }

    fn dir_purgem(&mut self, cur: &mut Cursor<'_>) -> bool {
        loop {
            let Some((name, span)) = self.expect_name(cur) else {
                return true;
            };
            let lowered = self.interner.get(name).to_ascii_lowercase();
            let key = self.interner.intern(&lowered);
            if self.macros.remove(&key).is_none() {
                self.diags
                    .error(span, format!("no macro named `{lowered}` to purge"));
            }
            if cur.eat_punct(Punct::Comma).is_none() {
                break;
            }
        }
        true
    }

    fn dir_include(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let Some(name) = self.expect_string(cur, "file name") else {
            return true;
        };
        let name = String::from_utf8_lossy(&name).into_owned();
        let Some(path) = self.find_include(&name) else {
            self.diags
                .error(span, format!("cannot find include file `{name}`"));
            return true;
        };
        self.include(&path, span);
        true
    }

    /// `.gnu_attribute <tag>, <value>`: one of the object's vendor-neutral
    /// build attributes, which on PowerPC is where the floating-point and
    /// vector ABIs a linker refuses to mix are recorded. Every ELF target
    /// has the directive and none writes these tags of its own accord.
    ///
    /// They land beside the processor's where the target has a
    /// build-attributes section of its own, as they do in GNU as for ARM and
    /// RISC-V, and in a `.gnu.attributes` where it has none, as on MIPS and
    /// PowerPC; see `Assembler::attribute_sections`. GNU as for MSP430 drops
    /// them instead, which looks like an oversight rather than a rule, so
    /// rsasm writes them there too.
    ///
    /// The tag is a number, and the value a number or a string.
    fn dir_gnu_attribute(&mut self, cur: &mut Cursor<'_>, span: Span) -> bool {
        let tok = cur.peek();
        let TokKind::Int(tag) = tok.kind else {
            self.diags
                .error(tok.span, "`.gnu_attribute` expects a tag number");
            cur.set_pos(cur.all().len());
            return true;
        };
        if tag > u64::from(u32::MAX) {
            self.diags
                .error(tok.span, "`.gnu_attribute` tag is too large");
            cur.set_pos(cur.all().len());
            return true;
        }
        cur.advance();
        if !cur.peek().is_punct(Punct::Comma) {
            self.diags
                .error(span, "`.gnu_attribute` expects a comma and a value");
            cur.set_pos(cur.all().len());
            return true;
        }
        cur.advance();
        let tok = cur.peek();
        let value = match tok.kind {
            TokKind::Int(n) => crate::arch::AttrValue::Int(n),
            TokKind::Str(i) => {
                crate::arch::AttrValue::Str(String::from_utf8_lossy(self.pool.get(i)).into_owned())
            }
            _ => {
                self.diags
                    .error(tok.span, "`.gnu_attribute` expects a number or a string");
                cur.set_pos(cur.all().len());
                return true;
            }
        };
        cur.advance();
        self.attr_overrides
            .retain(|&(v, t, _)| (v, t) != ("gnu", tag as u32));
        self.attr_overrides.push(("gnu", tag as u32, value));
        true
    }

    fn dir_arch(&mut self, cur: &mut Cursor<'_>, span: Span, cpu: bool) -> bool {
        let tok = cur.peek();
        let name = match tok.kind {
            TokKind::Str(i) => {
                cur.advance();
                String::from_utf8_lossy(self.pool.get(i)).into_owned()
            }
            // A bare name is whatever is written without spaces, since names
            // like `68000`, `78k0` and `x86-64` are not single identifiers.
            // GNU as for m68k lists extensions after commas, `68000,68881`,
            // so the whole run is offered first and the name before the first
            // comma after that.
            _ if !tok.is_eol() && !tok.is_punct(Punct::Comma) => {
                let mut first = None;
                let mut last = cur.advance();
                while !cur.peek().is_eol() && !cur.peek().preceded_by_space {
                    if cur.peek().is_punct(Punct::Comma) && first.is_none() {
                        first = Some((cur.pos(), last));
                    }
                    last = cur.advance();
                }
                let whole = self.sm.span_text(tok.span.to(last.span)).to_string();
                match first {
                    Some((pos, before)) if crate::arch::lookup(&whole).is_none() => {
                        cur.set_pos(pos);
                        self.sm.span_text(tok.span.to(before.span)).to_string()
                    }
                    _ => whole,
                }
            }
            _ => {
                self.diags.error(tok.span, "expected an architecture name");
                return true;
            }
        };
        // On ARM the directive names a CPU of the backend's own, which only
        // changes what the build attributes say; see
        // `Architecture::selects_cpu`.
        let mut state = self.arch_state.clone();
        match self
            .arch
            .selects_cpu(&mut state, &name.to_ascii_lowercase(), cpu)
        {
            Ok(true) => {
                self.arch_state = state;
                return true;
            }
            Err(msg) => {
                self.diags.error(span, msg);
                return true;
            }
            Ok(false) => {}
        }
        match crate::arch::lookup(&name) {
            Some(a) => self.switch_arch(a),
            None => {
                let avail = crate::arch::available().join(", ");
                self.diags.emit(
                    crate::diag::Diagnostic::error(span, format!("unknown architecture `{name}`"))
                        .with_help(format!("this build supports: {avail}")),
                );
            }
        }
        true
    }
}

/// Whether `name` is one of GNU as's built-in section names, or a subsection
/// of it: `.tdata` and `.tdata.foo` are the thread-local data section, while
/// `.tdatax` is a section the source invented and gets no flags of its own.
fn builtin_section(name: &str, base: &str) -> bool {
    name == base || name.strip_prefix(base).is_some_and(|r| r.starts_with('.'))
}

/// What a section holds and how it is mapped, by name alone, for the names
/// GNU as knows; `None` for a name the source invented, which starts with
/// nothing.
fn builtin_section_kind(name: &str) -> Option<(SectionKind, SectionFlags)> {
    // A thread-local section is written data that the linker copies into
    // each thread's block rather than mapping, so it carries `SHF_TLS` on
    // top of what `.data` and `.bss` carry.
    let tls = SectionFlags {
        tls: true,
        ..SectionFlags::data()
    };
    Some(match name {
        _ if builtin_section(name, ".text") || name == ".init" || name == ".fini" => {
            (SectionKind::Progbits, SectionFlags::text())
        }
        _ if builtin_section(name, ".rodata") || name == ".rodata1" => {
            (SectionKind::Progbits, SectionFlags::rodata())
        }
        _ if builtin_section(name, ".tdata") => (SectionKind::Progbits, tls),
        _ if builtin_section(name, ".tbss") => (SectionKind::Nobits, tls),
        _ if builtin_section(name, ".data") || name == ".data1" => {
            (SectionKind::Progbits, SectionFlags::data())
        }
        _ if builtin_section(name, ".bss") => (SectionKind::Nobits, SectionFlags::data()),
        _ => return None,
    })
}

/// The flags a built-in section gets when `.section` writes some of its own.
///
/// GNU as adds the name's own flags to what was written where the two agree,
/// so `.section .text.hot,"a"` is still executable and
/// `.section .rodata.str1.1,"aMS"` is still read-only; where they do not, it
/// warns and takes what was written, so `.section .init,"aw"` is a writable
/// section that nothing executes. `M` and `S` describe the contents rather
/// than the mapping and never count as a disagreement, nor do the flags
/// rsasm has no bit for (`G`, `R` and `e`).
fn builtin_section_flags(builtin: SectionFlags, written: SectionFlags) -> SectionFlags {
    let disagrees = (written.alloc && !builtin.alloc)
        || (written.write && !builtin.write)
        || (written.exec && !builtin.exec)
        || (written.tls && !builtin.tls);
    if disagrees {
        written
    } else {
        SectionFlags {
            merge: written.merge,
            strings: written.strings,
            group: written.group,
            ..builtin
        }
    }
}

/// The width a `.dc`, `.dcb` or `.ds` suffix names, a word where it has none.
fn motorola_width(directive: &str) -> u8 {
    match directive.rsplit_once('.') {
        Some((_, "b")) => 1,
        Some((_, "l")) => 4,
        _ => 2,
    }
}

fn parse_flags(s: &str) -> SectionFlags {
    let mut f = SectionFlags::default();
    for c in s.chars() {
        match c {
            'a' => f.alloc = true,
            'w' => f.write = true,
            'x' => f.exec = true,
            'M' => f.merge = true,
            'S' => f.strings = true,
            'T' => f.tls = true,
            'G' => f.group = true,
            _ => {}
        }
    }
    f
}
