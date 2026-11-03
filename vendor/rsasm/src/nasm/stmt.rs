//! NASM statements, once the preprocessor is done with a line.
//!
//! What NASM's parser reads: an optional label, with or without a colon;
//! `times`; a pseudo-instruction (`db`, `resb`, `incbin`, `equ`) or an
//! instruction; or a primitive directive in brackets, `[section .data]`.
//!
//! Labels follow NASM's rules. One that starts with a single `.` belongs to
//! the last label that did not, so `.loop` after `strlen:` is `strlen.loop`;
//! `..@` names, which `%%` and `%$` expand to, belong to nobody and change
//! nothing. In an `absolute` block a label is a number, which is how `struc`
//! gives its fields their offsets.

use super::{Absolute, Unwind};
use crate::assembler::Assembler;
use crate::cursor::Cursor;
use crate::expr::{BinOp, ExprKind, Value};
use crate::intern::Name;
use crate::lexer::{Punct, TokKind, Token};
use crate::parser::{LabelDef, Statement};
use crate::section::{FragKind, Fragment, SectionFlags, SectionKind, Variant};
use crate::source::Span;
use crate::symbol::{Binding, SymType, SymbolValue, Visibility};

/// Where `$` and `$$` are, for the statement being assembled.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Here {
    /// A label at the statement, in a real section.
    Label(crate::symbol::SymbolId),
    /// Inside `absolute`: numbers.
    Absolute(Absolute),
}

/// The pseudo-instructions, which are never labels.
const PSEUDO: &[&str] = &[
    "db", "dw", "dd", "dq", "dt", "do", "dy", "dz", "resb", "resw", "resd", "resq", "rest", "reso",
    "resy", "resz", "incbin", "equ", "times",
];

/// The instruction prefixes NASM knows, which are never labels either.
const PREFIXES: &[&str] = &[
    "lock", "rep", "repe", "repz", "repne", "repnz", "a16", "a32", "a64", "o16", "o32", "o64",
    "es", "cs", "ss", "ds", "fs", "gs", "bnd", "nobnd", "wait",
];

impl Assembler {
    /// Assembles one preprocessed NASM line.
    pub(crate) fn nasm_statement(&mut self, stmt: &Statement) {
        let toks = &stmt.toks;
        if toks.is_empty() {
            return;
        }
        if toks[0].is_punct(Punct::LBracket) {
            self.nasm_bracketed(toks, stmt.span);
            return;
        }
        let mut i = 0;
        let mut label: Option<(Name, Span)> = None;
        if let TokKind::Ident(n) = toks[0].kind {
            if toks.get(1).is_some_and(|t| t.is_punct(Punct::Colon)) {
                label = Some((n, toks[0].span));
                i = 2;
            } else if !self.nasm_is_keyword(n) {
                label = Some((n, toks[0].span));
                i = 1;
            }
        }
        let word = toks
            .get(i)
            .and_then(|t| t.ident())
            .map(|n| self.interner.get(n).to_ascii_lowercase());
        if word.as_deref() == Some("equ") {
            match label {
                Some((name, span)) => self.nasm_equ(name, span, &toks[i + 1..], stmt.span),
                None => self.diags.error(stmt.span, "`equ` needs a label to define"),
            }
            return;
        }
        if let Some((name, span)) = label {
            self.nasm_define_label(name, span);
        }
        if i < toks.len() {
            self.nasm_body(&toks[i..], stmt.span);
        }
    }

    /// Whether a word at the start of a line is an instruction, prefix or
    /// pseudo-instruction rather than a label.
    fn nasm_is_keyword(&self, n: Name) -> bool {
        let word = self.interner.get(n).to_ascii_lowercase();
        PSEUDO.contains(&word.as_str())
            || PREFIXES.contains(&word.as_str())
            || self.arch.is_mnemonic(&word)
    }

    /// The name a label is defined under: a `.local` one is qualified by the
    /// last label that was not.
    pub(crate) fn nasm_label_name(&self, text: &str) -> Option<String> {
        if text.starts_with('.') && !text.starts_with("..") {
            return self
                .nasm
                .base_label
                .as_ref()
                .map(|base| format!("{base}{text}"));
        }
        None
    }

    fn nasm_define_label(&mut self, name: Name, span: Span) {
        let text = self.interner.get(name).to_string();
        let full = match self.nasm_label_name(&text) {
            Some(q) => self.interner.intern(&q),
            None => {
                if !text.starts_with('.') {
                    self.nasm.base_label = Some(text.clone());
                }
                name
            }
        };
        if text.starts_with("..") && !text.starts_with("..@") {
            // `..start` and the other special symbols mean something only to
            // object formats rsasm does not write.
            self.diags.error(
                span,
                format!("`{text}` is a special symbol and cannot be defined here"),
            );
            return;
        }
        match self.nasm.absolute {
            Some(abs) => {
                let e = self.exprs.int(abs.here as u64, span);
                self.set_symbol(full, e, span);
                let id = self.symbols.intern(full, span);
                self.symbols.get_mut(id).redefinable = false;
            }
            None => {
                self.define_label(&LabelDef::Named(full, span));
            }
        }
    }

    fn nasm_equ(&mut self, name: Name, name_span: Span, toks: &[Token], span: Span) {
        let text = self.interner.get(name).to_string();
        let full = match self.nasm_label_name(&text) {
            Some(q) => self.interner.intern(&q),
            None => name,
        };
        let toks = self.nasm_rewrite_locals(toks);
        let here = self.nasm_here_for(&toks, span);
        let mut cur = Cursor::new(&toks);
        let mark = self.exprs.len();
        let Some(e) = self.parse_expr(&mut cur) else {
            return;
        };
        self.expect_end(&mut cur);
        self.nasm_bind_positional(mark, here);
        let id = self.symbols.intern(full, name_span);
        if self.symbols.get(id).is_defined() {
            let prev = self.symbols.get(id).def_span;
            self.diags.emit(
                crate::diag::Diagnostic::error(
                    name_span,
                    format!("symbol `{}` is already defined", self.interner.get(full)),
                )
                .with_note(prev, "previous definition is here"),
            );
            return;
        }
        // Fold what already has a value, so a later redefinition of a
        // `%assign`-style name cannot reach back.
        let e = match self.eval_ref(e).ok().and_then(|v| v.as_abs()) {
            Some(v) => self.exprs.int(v as u64, span),
            None => e,
        };
        self.set_symbol(full, e, name_span);
        self.symbols.get_mut(id).redefinable = false;
    }

    /// Qualifies `.local` names in an expression's tokens.
    pub(crate) fn nasm_rewrite_locals(&mut self, toks: &[Token]) -> Vec<Token> {
        toks.iter()
            .map(|t| {
                let TokKind::Ident(n) = t.kind else {
                    return *t;
                };
                match self.nasm_label_name(self.interner.get(n)) {
                    Some(q) => Token {
                        kind: TokKind::Ident(self.interner.intern(&q)),
                        ..*t
                    },
                    None => *t,
                }
            })
            .collect()
    }

    /// `$` and `$$` for a statement, if its tokens mention them.
    fn nasm_here_for(&mut self, toks: &[Token], span: Span) -> Option<Here> {
        if !toks.iter().any(|t| t.is_punct(Punct::Dollar)) {
            return None;
        }
        Some(self.nasm_here(span))
    }

    fn nasm_here(&mut self, span: Span) -> Here {
        match self.nasm.absolute {
            Some(a) => Here::Absolute(a),
            None => Here::Label(self.anon_label(span)),
        }
    }

    /// Binds the `$` and `$$` nodes parsed since `mark`.
    pub(crate) fn nasm_bind_positional(&mut self, mark: usize, here: Option<Here>) {
        for i in mark..self.exprs.nodes.len() {
            let span = self.exprs.nodes[i].span;
            let new = match (&self.exprs.nodes[i].kind, here) {
                (ExprKind::Here, Some(Here::Label(id))) => ExprKind::SymId(id),
                (ExprKind::Here, Some(Here::Absolute(a))) => ExprKind::Int(a.here as u64),
                (ExprKind::SectionStart, Some(Here::Absolute(a))) => ExprKind::Int(a.base as u64),
                (ExprKind::SectionStart, Some(Here::Label(_))) => {
                    ExprKind::SymId(self.nasm_section_start())
                }
                (ExprKind::Here | ExprKind::SectionStart, None) => {
                    self.diags.error(span, "`$` cannot be used here");
                    ExprKind::Int(0)
                }
                _ => continue,
            };
            self.exprs.nodes[i].kind = new;
        }
    }

    /// The label at the start of the current section, which `$$` is.
    fn nasm_section_start(&mut self) -> crate::symbol::SymbolId {
        let cur = self.cur;
        if let Some(&id) = self.nasm.section_starts.get(&cur) {
            return id;
        }
        let n = self.symbols.len();
        let name = self.interner.intern(&format!(".L\u{0}sectstart.{n}"));
        let id = self.symbols.intern(name, Span::DUMMY);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Label {
            section: cur,
            frag: 0,
        };
        self.symbols.mark_defined(id);
        self.nasm.section_starts.insert(cur, id);
        id
    }

    /// The number a difference of two labels already is, when nothing that
    /// can change size lies between them; NASM would know it by the pass
    /// that needs it.
    pub(crate) fn nasm_fixed_value(&self, v: Value) -> Option<i64> {
        let (Some(p), Some(m)) = (v.plus, v.minus) else {
            return None;
        };
        let pos = |id| match self.symbols.get(id).value {
            SymbolValue::Label { section, frag } => Some((section, frag)),
            _ => None,
        };
        let (ps, pf) = pos(p)?;
        let (ms, mf) = pos(m)?;
        if ps != ms {
            return None;
        }
        let (lo, hi, sign) = if mf <= pf { (mf, pf, 1) } else { (pf, mf, -1) };
        let frags = &self.section(ps).frags;
        let mut total = 0i64;
        for f in frags.get(lo as usize..(hi as usize).min(frags.len()))? {
            if f.relaxable {
                return None;
            }
            total += match &f.kind {
                FragKind::Bytes { variants, .. } if variants.len() == 1 => {
                    variants[0].bytes.len() as i64
                }
                FragKind::Space { size, .. } => {
                    self.eval_ref(*size).ok()?.as_abs().filter(|n| *n >= 0)?
                }
                _ => return None,
            };
        }
        Some(sign * total + v.addend)
    }

    // ---- the statement after its label ---------------------------------

    fn nasm_body(&mut self, toks: &[Token], span: Span) {
        let Some(n) = toks[0].ident() else {
            self.diags
                .error(toks[0].span, "expected a label, instruction or directive");
            return;
        };
        let word = self.interner.get(n).to_ascii_lowercase();
        match word.as_str() {
            "times" => self.nasm_times(&toks[1..], span),
            "db" => self.nasm_data(1, &toks[1..], span, None),
            "dw" => self.nasm_data(2, &toks[1..], span, None),
            "dd" => self.nasm_data(4, &toks[1..], span, None),
            "dq" => self.nasm_data(8, &toks[1..], span, None),
            "dt" => self.nasm_data(10, &toks[1..], span, None),
            "do" => self.nasm_data(16, &toks[1..], span, None),
            "dy" => self.nasm_data(32, &toks[1..], span, None),
            "dz" => self.nasm_data(64, &toks[1..], span, None),
            "resb" => self.nasm_reserve(1, &toks[1..], span),
            "resw" => self.nasm_reserve(2, &toks[1..], span),
            "resd" => self.nasm_reserve(4, &toks[1..], span),
            "resq" => self.nasm_reserve(8, &toks[1..], span),
            "rest" => self.nasm_reserve(10, &toks[1..], span),
            "reso" => self.nasm_reserve(16, &toks[1..], span),
            "resy" => self.nasm_reserve(32, &toks[1..], span),
            "resz" => self.nasm_reserve(64, &toks[1..], span),
            "incbin" => self.nasm_incbin(&toks[1..], span),
            "equ" => self.diags.error(span, "`equ` needs a label to define"),
            _ => self.nasm_instruction(toks, span, None),
        }
    }

    fn nasm_no_code_in_absolute(&mut self, span: Span) -> bool {
        if self.nasm.absolute.is_some() {
            self.diags
                .error(span, "attempt to assemble code in `absolute` space");
            return true;
        }
        false
    }

    fn nasm_instruction(&mut self, toks: &[Token], span: Span, here: Option<Here>) {
        if self.nasm_no_code_in_absolute(span) {
            return;
        }
        let toks = self.nasm_rewrite_locals(toks);
        let Some(mnemonic) = toks[0].ident() else {
            self.diags.error(toks[0].span, "expected an instruction");
            return;
        };
        let lowered = self
            .interner
            .intern(&self.interner.get(mnemonic).to_ascii_lowercase());
        let here = match here {
            Some(h) => Some(h),
            None => self.nasm_here_for(&toks, span),
        };
        let mark = self.exprs.len();
        self.instruction_tokens(&toks[1..], lowered, toks[0].span, span);
        self.nasm_bind_positional(mark, here);
    }

    fn nasm_times(&mut self, toks: &[Token], span: Span) {
        let toks = self.nasm_rewrite_locals(toks);
        let here = self.nasm_here_for(&toks, span);
        let mut cur = Cursor::new(&toks);
        let mark = self.exprs.len();
        let Some(count) = self.parse_expr(&mut cur) else {
            return;
        };
        self.nasm_bind_positional(mark, here);
        let rest = cur.rest();
        if rest.is_empty() {
            self.diags.error(span, "`times` needs something to repeat");
            return;
        }
        let known = self
            .eval(count)
            .ok()
            .and_then(|v| v.as_abs().or_else(|| self.nasm_fixed_value(v)));
        if let Some(n) = known {
            if n < 0 {
                self.diags
                    .error(span, format!("`times` count {n} is negative"));
                return;
            }
            if n > 1 << 28 {
                self.diags
                    .error(span, format!("`times` count {n} is too large"));
                return;
            }
            if let Some(bytes) = self.nasm_constant_bytes(rest, span) {
                if !self.nasm_no_code_in_absolute(span) {
                    let all = bytes.repeat(n as usize);
                    self.emit_bytes(&all, span);
                }
                return;
            }
            for _ in 0..n {
                self.nasm_repeat_body(rest, span, here);
                if self.diags.saturated() || self.nasm.unwind == Some(Unwind::Macro) {
                    break;
                }
            }
            return;
        }
        // A count only layout can know, such as `510-($-$$)` after a jump:
        // a run of one byte value becomes space filled with it.
        let Some(bytes) = self.nasm_constant_bytes(rest, span) else {
            self.diags.error(
                span,
                "`times` count is not a constant here, and what it repeats is not a run of \
                 one byte value",
            );
            return;
        };
        if self.nasm_no_code_in_absolute(span) {
            return;
        }
        let Some(&fill_byte) = bytes.first() else {
            return;
        };
        if bytes.iter().any(|&b| b != fill_byte) {
            self.diags.error(
                span,
                "`times` count is not a constant here, and what it repeats is not a run of \
                 one byte value",
            );
            return;
        }
        let size = if bytes.len() == 1 {
            count
        } else {
            let w = self.exprs.int(bytes.len() as u64, span);
            self.exprs
                .alloc(ExprKind::Binary(BinOp::Mul, count, w), span)
        };
        let fill = self.exprs.int(fill_byte as u64, span);
        if self.section(self.cur).kind == SectionKind::Nobits && fill_byte != 0 {
            self.check_nobits(span);
            return;
        }
        self.cur_section().push(Fragment::new(
            FragKind::Space {
                size,
                fill,
                resolved: 0,
            },
            span,
        ));
    }

    /// One repetition of what `times` repeats, where `$` is where `times`
    /// began.
    fn nasm_repeat_body(&mut self, toks: &[Token], span: Span, here: Option<Here>) {
        let Some(n) = toks[0].ident() else {
            self.diags.error(toks[0].span, "expected an instruction");
            return;
        };
        let word = self.interner.get(n).to_ascii_lowercase();
        let width = match word.as_str() {
            "db" => 1,
            "dw" => 2,
            "dd" => 4,
            "dq" => 8,
            "dt" => 10,
            "do" => 16,
            "dy" => 32,
            "dz" => 64,
            "times" => {
                self.nasm_times(&toks[1..], span);
                return;
            }
            "resb" | "resw" | "resd" | "resq" | "rest" | "reso" | "resy" | "resz" | "incbin" => {
                self.nasm_body(toks, span);
                return;
            }
            _ => {
                self.nasm_instruction(toks, span, here);
                return;
            }
        };
        self.nasm_data(width, &toks[1..], span, here);
    }

    /// The bytes a statement assembles to, if it is data or an instruction
    /// with no symbol in it and no choice of size.
    fn nasm_constant_bytes(&mut self, toks: &[Token], span: Span) -> Option<Vec<u8>> {
        if toks.iter().any(|t| t.is_punct(Punct::Dollar)) {
            return None;
        }
        let n = toks[0].ident()?;
        let word = self.interner.get(n).to_ascii_lowercase();
        let width = match word.as_str() {
            "db" => 1,
            "dw" => 2,
            "dd" => 4,
            "dq" => 8,
            _ => {
                // An instruction: only if it is one fixed sequence of bytes.
                if PSEUDO.contains(&word.as_str()) {
                    return None;
                }
                let diags = self.diags.len();
                let variants = self.nasm_try_instruction(toks, span)?;
                if self.diags.len() != diags {
                    return None;
                }
                return match variants.as_slice() {
                    [v] if v.fixups.is_empty() => Some(v.bytes.clone()),
                    _ => None,
                };
            }
        };
        let mut out = Vec::new();
        let cur = Cursor::new(&toks[1..]);
        for item in cur.split_commas() {
            match item {
                [t] if matches!(t.kind, TokKind::Str(_)) => {
                    let TokKind::Str(i) = t.kind else {
                        unreachable!()
                    };
                    let mut bytes = self.pool.get(i).to_vec();
                    pad_to(&mut bytes, width);
                    out.extend(bytes);
                }
                _ => {
                    let mut c = Cursor::new(item);
                    let mark = self.exprs.len();
                    let diags = self.diags.len();
                    let e = self.parse_expr(&mut c);
                    let v = e
                        .and_then(|e| self.eval_ref(e).ok())
                        .and_then(|v| v.as_abs());
                    self.exprs.nodes.truncate(mark);
                    if self.diags.len() != diags || !c.at_end() {
                        self.truncate_diags(diags);
                        return None;
                    }
                    out.extend_from_slice(&(v? as u64).to_le_bytes()[..width]);
                }
            }
        }
        Some(out)
    }

    fn truncate_diags(&mut self, len: usize) {
        if self.diags.len() <= len {
            return;
        }
        let kept: Vec<_> = self.diags.take().into_iter().take(len).collect();
        for d in kept {
            self.diags.emit(d);
        }
    }

    /// The candidate encodings of an instruction, without emitting it.
    fn nasm_try_instruction(&mut self, toks: &[Token], span: Span) -> Option<Vec<Variant>> {
        let toks = self.nasm_rewrite_locals(toks);
        let mnemonic = toks[0].ident()?;
        let lowered = self
            .interner
            .intern(&self.interner.get(mnemonic).to_ascii_lowercase());
        self.assemble_instruction(&toks[1..], lowered, toks[0].span, span)
            .map(|(v, ..)| v)
    }

    // ---- data ------------------------------------------------------------

    fn nasm_data(&mut self, width: u8, toks: &[Token], span: Span, here: Option<Here>) {
        if self.nasm_no_code_in_absolute(span) {
            return;
        }
        let toks = self.nasm_rewrite_locals(toks);
        if toks.is_empty() {
            self.diags.error(span, "no operand for data declaration");
            return;
        }
        let here = match here {
            Some(h) => Some(h),
            None => self.nasm_here_for(&toks, span),
        };
        let cur = Cursor::new(&toks);
        for item in cur.split_commas() {
            if item.is_empty() {
                self.diags.error(span, "expected a value");
                return;
            }
            // A string that is a whole item is its bytes, padded to the
            // width; anywhere else a string is a number.
            if let [t] = item
                && let TokKind::Str(i) = t.kind
            {
                let mut bytes = self.pool.get(i).to_vec();
                if bytes.is_empty() {
                    continue;
                }
                pad_to(&mut bytes, width as usize);
                self.emit_bytes(&bytes, span);
                continue;
            }
            if let [t] = item
                && t.ident().is_some_and(|n| self.interner.get(n) == "?")
            {
                let zeros = vec![0u8; width as usize];
                self.emit_bytes(&zeros, span);
                continue;
            }
            if let Some(bytes) = self.nasm_float(item, width) {
                self.emit_bytes(&bytes, span);
                continue;
            }
            if width > 8 {
                self.diags.error(
                    item[0].span,
                    format!("a {width}-byte data item must be a floating-point constant"),
                );
                return;
            }
            let mut c = Cursor::new(item);
            let mark = self.exprs.len();
            let Some(e) = self.parse_expr(&mut c) else {
                return;
            };
            if !c.at_end() {
                self.diags
                    .error(c.peek().span, "comma expected after operand");
                return;
            }
            self.nasm_bind_positional(mark, here);
            if let Some(v) = self.eval_ref(e).ok().and_then(|v| v.as_abs()) {
                let kind = crate::section::FixupKind::data(width);
                if !kind.fits(v as i128) {
                    let espan = self.exprs.span(e);
                    self.diags.warning(
                        espan,
                        format!("{}-bit data exceeds bounds", width as u32 * 8),
                    );
                }
                let bytes = (v as u64).to_le_bytes();
                self.emit_bytes(&bytes[..width as usize], span);
                continue;
            }
            self.emit_value(width, e, span);
        }
    }

    /// A floating-point data item, as IEEE bytes of `width`.
    fn nasm_float(&mut self, item: &[Token], width: u8) -> Option<Vec<u8>> {
        let (neg, tok) = match item {
            [t] => (false, t),
            [s, t] if s.is_punct(Punct::Minus) => (true, t),
            [s, t] if s.is_punct(Punct::Plus) => (false, t),
            _ => return None,
        };
        let TokKind::BadNumber(n) = tok.kind else {
            return None;
        };
        let text = self.interner.get(n).replace('_', "");
        let Ok(v) = text.parse::<f64>() else {
            self.diags.error(
                tok.span,
                format!("invalid floating-point constant `{text}`"),
            );
            return Some(Vec::new());
        };
        let v = if neg { -v } else { v };
        Some(match width {
            4 => (v as f32).to_bits().to_le_bytes().to_vec(),
            8 => v.to_bits().to_le_bytes().to_vec(),
            10 => extended_bytes(v).to_vec(),
            _ => {
                self.diags.error(
                    tok.span,
                    format!("a floating-point constant cannot be {width} bytes wide"),
                );
                Vec::new()
            }
        })
    }

    fn nasm_reserve(&mut self, width: u64, toks: &[Token], span: Span) {
        let toks = self.nasm_rewrite_locals(toks);
        let here = self.nasm_here_for(&toks, span);
        let mut cur = Cursor::new(&toks);
        let mark = self.exprs.len();
        let Some(count) = self.parse_expr(&mut cur) else {
            return;
        };
        self.expect_end(&mut cur);
        self.nasm_bind_positional(mark, here);
        if let Some(abs) = self.nasm.absolute.as_mut() {
            let _ = abs;
            let Some(n) = self.eval_absolute(count, "`res` count in `absolute` space") else {
                return;
            };
            if let Some(abs) = self.nasm.absolute.as_mut() {
                abs.here += n * width as i64;
            }
            return;
        }
        let size = if width == 1 {
            count
        } else {
            let w = self.exprs.int(width, span);
            self.exprs
                .alloc(ExprKind::Binary(BinOp::Mul, count, w), span)
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

    fn nasm_incbin(&mut self, toks: &[Token], span: Span) {
        if self.nasm_no_code_in_absolute(span) {
            return;
        }
        let cur = Cursor::new(toks);
        let items = cur.split_commas();
        let name = match items.first() {
            Some([t]) if matches!(t.kind, TokKind::Str(_)) => {
                let TokKind::Str(i) = t.kind else {
                    unreachable!()
                };
                String::from_utf8_lossy(self.pool.get(i)).into_owned()
            }
            _ => {
                self.diags
                    .error(span, "`incbin` expects a file name in quotes");
                return;
            }
        };
        let mut numbers = Vec::new();
        for item in &items[1..] {
            let mut c = Cursor::new(item);
            let Some(e) = self.parse_expr(&mut c) else {
                return;
            };
            let Some(v) = self.eval_absolute(e, "`incbin` offset and length") else {
                return;
            };
            numbers.push(v.max(0) as usize);
        }
        let Some(path) = self.find_include(&name) else {
            self.diags
                .error(span, format!("`incbin`: unable to open file `{name}`"));
            return;
        };
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                self.diags
                    .error(span, format!("cannot read `{}`: {e}", path.display()));
                return;
            }
        };
        let skip = numbers.first().copied().unwrap_or(0).min(data.len());
        let end = match numbers.get(1) {
            Some(&len) => (skip + len).min(data.len()),
            None => data.len(),
        };
        self.emit_bytes(&data[skip..end], span);
    }

    // ---- primitive directives -------------------------------------------

    fn nasm_bracketed(&mut self, toks: &[Token], span: Span) {
        let Some(close) = toks.iter().rposition(|t| t.is_punct(Punct::RBracket)) else {
            self.diags.error(span, "expected `]` to close a directive");
            return;
        };
        let Some(name) = toks.get(1).and_then(|t| t.ident()) else {
            self.diags.error(span, "expected a directive name");
            return;
        };
        if close + 1 != toks.len() {
            self.diags.error(
                toks[close + 1].span,
                "unexpected tokens after the directive",
            );
        }
        let word = self.interner.get(name).to_ascii_lowercase();
        let args = &toks[2..close];
        match word.as_str() {
            "section" | "segment" => self.nasm_section(args, span),
            "absolute" => self.nasm_absolute(args, span),
            "bits" => self.nasm_bits(args, span),
            "global" => self.nasm_symbol_directive(args, span, Binding::Global),
            "static" => self.nasm_symbol_directive(args, span, Binding::Local),
            "extern" => self.nasm_extern(args, span),
            "common" => self.nasm_common(args, span),
            "org" => self.nasm_org(args, span),
            "sectalign" => {
                let mut cur = Cursor::new(args);
                if let Some(e) = self.parse_expr(&mut cur)
                    && let Some(n) = self.eval_absolute(e, "`sectalign` value")
                {
                    if n > 0 && (n as u64).is_power_of_two() {
                        let id = self.cur;
                        if self.nasm.absolute.is_none() {
                            // In a flat binary a section has no alignment of
                            // its own until something asks for one: NASM
                            // starts each at a multiple of four and then
                            // takes the first request as the alignment, even
                            // where it is smaller than four. An object's
                            // section is aligned by its kind from the start,
                            // and a request only ever raises that.
                            let first = !self.options.relocatable
                                && !self.nasm.explicit_align.contains(&id);
                            let s = self.section_mut(id);
                            s.align = if first {
                                n as u64
                            } else {
                                s.align.max(n as u64)
                            };
                            self.nasm.explicit_align.insert(id);
                        }
                    } else {
                        self.diags
                            .error(span, format!("`sectalign` value {n} is not a power of two"));
                    }
                }
            }
            "default" => {
                for t in args {
                    let Some(n) = t.ident() else {
                        continue;
                    };
                    match self.interner.get(n).to_ascii_lowercase().as_str() {
                        "rel" => self.nasm.default_rel = true,
                        "abs" => self.nasm.default_rel = false,
                        "bnd" | "nobnd" => {}
                        other => self
                            .diags
                            .error(t.span, format!("unknown `default` setting `{other}`")),
                    }
                }
                self.nasm_sync_default_rel();
            }
            "cpu" | "float" | "warning" | "list" | "required" | "pragma" | "debug" | "osabi" => {}
            _ => self
                .diags
                .error(span, format!("unrecognised directive `[{word}]`")),
        }
    }

    /// Tells the backend about `default rel`.
    pub(crate) fn nasm_sync_default_rel(&mut self) {
        const DEFAULT_REL: u64 = crate::arch::FEATURE_DEFAULT_REL;
        if self.nasm.default_rel {
            self.arch_state.features |= DEFAULT_REL;
        } else {
            self.arch_state.features &= !DEFAULT_REL;
        }
    }

    fn nasm_bits(&mut self, args: &[Token], span: Span) {
        let bits = match args {
            [t] => match t.kind {
                TokKind::Int(n @ (16 | 32 | 64)) => n,
                _ => 0,
            },
            _ => 0,
        };
        if bits == 0 {
            self.diags.error(span, "`bits` expects 16, 32 or 64");
            return;
        }
        let name = format!(".code{bits}");
        if !self.arch_directive_tokens(&name, &[]) {
            self.diags.error(
                span,
                format!("the `{}` backend has no {bits}-bit mode", self.arch.name()),
            );
        }
    }

    fn nasm_section(&mut self, args: &[Token], span: Span) {
        let Some(first) = args.first() else {
            self.diags.error(span, "expected a section name");
            return;
        };
        // A section name runs to the first blank, so `.text.startup` and
        // `.data.rel.ro` are single names even though they lex as several.
        let mut end = 1;
        while end < args.len() && !args[end].preceded_by_space {
            end += 1;
        }
        let name = match first.kind {
            TokKind::Str(i) if end == 1 => String::from_utf8_lossy(self.pool.get(i)).into_owned(),
            _ => self
                .sm
                .span_text(first.span.to(args[end - 1].span))
                .to_string(),
        };
        if self.options.format.is_coff() {
            self.nasm_coff_section(name, &args[end..], span);
            return;
        }
        let relocatable = self.options.relocatable;
        let (mut kind, mut flags, mut align) = if relocatable {
            elf_section_defaults(&name)
        } else {
            let nobits = name == ".bss";
            (
                if nobits {
                    SectionKind::Nobits
                } else {
                    SectionKind::Progbits
                },
                SectionFlags {
                    alloc: true,
                    write: nobits,
                    exec: name == ".text",
                    ..Default::default()
                },
                // NASM's flat output starts every section it is told about at
                // a multiple of four, unless `align=` says otherwise.
                4,
            )
        };
        let mut explicit_align = false;
        let mut i = end;
        while i < args.len() {
            let t = args[i];
            i += 1;
            let Some(n) = t.ident() else {
                self.diags.error(t.span, "expected a section attribute");
                return;
            };
            let attr = self.interner.get(n).to_ascii_lowercase();
            // `align=16`, `start=0x100` and the like.
            let value = if args.get(i).is_some_and(|t| t.is_punct(Punct::Eq)) {
                i += 1;
                let mut j = i;
                while j < args.len() && !(args[j].preceded_by_space && j > i) {
                    j += 1;
                }
                let mut c = Cursor::new(&args[i..j]);
                i = j;
                let Some(e) = self.parse_expr(&mut c) else {
                    return;
                };
                self.eval_absolute(e, "a section attribute")
            } else {
                None
            };
            match attr.as_str() {
                "progbits" => kind = SectionKind::Progbits,
                "nobits" => kind = SectionKind::Nobits,
                "note" => kind = SectionKind::Note,
                "alloc" => flags.alloc = true,
                "noalloc" => flags.alloc = false,
                "exec" => flags.exec = true,
                "noexec" => flags.exec = false,
                "write" => flags.write = true,
                "nowrite" => flags.write = false,
                "tls" => flags.tls = true,
                "notls" => flags.tls = false,
                "merge" => flags.merge = true,
                "strings" => flags.strings = true,
                "align" => match value {
                    Some(v) if v > 0 && (v as u64).is_power_of_two() => {
                        align = v as u64;
                        explicit_align = true;
                    }
                    _ => {
                        self.diags
                            .error(t.span, "section alignment must be a power of two");
                        return;
                    }
                },
                "start" | "vstart" | "follows" | "vfollows" | "valign" => {
                    self.diags.error(
                        t.span,
                        format!("the `{attr}` section attribute is not supported"),
                    );
                    return;
                }
                _ => {
                    self.diags
                        .error(t.span, format!("unknown section attribute `{attr}`"));
                    return;
                }
            }
        }
        let n = self.interner.intern(&name);
        let existed = self.sections.iter().any(|s| s.name == n);
        let id = self.get_or_create_section(n, kind, flags, align);
        // The first `align=` on a flat binary's section replaces the writer's
        // own four rather than being measured against it; see `sectalign`.
        let first = !relocatable && !self.nasm.explicit_align.contains(&id);
        if !existed {
            self.section_mut(id).align = align;
        } else if explicit_align {
            let s = self.section_mut(id);
            s.align = if first { align } else { s.align.max(align) };
        }
        if explicit_align {
            self.nasm.explicit_align.insert(id);
        }
        self.nasm.opened.insert(id);
        self.set_section(id);
        self.nasm.absolute = None;
    }

    /// `section` in a `win32` or `win64` object, as NASM's COFF writer reads
    /// it: one of the words `code` (or `text`), `data`, `rdata`, `bss` and
    /// `info` choosing the characteristics outright, and `align=`. Without a
    /// word, the name decides, and a name NASM does not know is code.
    fn nasm_coff_section(&mut self, name: String, args: &[Token], span: Span) {
        use crate::output::coff as c;
        let win64 = c::machine(self.target()) == Some(c::MACHINE_AMD64);
        let mut chosen = None;
        let mut align = None;
        let mut i = 0;
        while i < args.len() {
            let t = args[i];
            i += 1;
            let Some(n) = t.ident() else {
                self.diags.error(t.span, "expected a section attribute");
                return;
            };
            let attr = self.interner.get(n).to_ascii_lowercase();
            match attr.as_str() {
                "code" | "text" => chosen = Some(NASM_TEXT),
                "data" => chosen = Some(NASM_DATA),
                "rdata" => chosen = Some(NASM_RDATA),
                "bss" => chosen = Some(NASM_BSS),
                "info" => chosen = Some(NASM_INFO),
                "align" if args.get(i).is_some_and(|t| t.is_punct(Punct::Eq)) => {
                    i += 1;
                    let mut j = i;
                    while j < args.len() && !(args[j].preceded_by_space && j > i) {
                        j += 1;
                    }
                    let mut c = Cursor::new(&args[i..j]);
                    i = j;
                    let Some(e) = self.parse_expr(&mut c) else {
                        return;
                    };
                    let v: Option<u64> = self
                        .eval_absolute(e, "a section alignment")
                        .map(|v| v.max(-1) as u64);
                    match v {
                        Some(0) => align = Some(None),
                        Some(v) if v.is_power_of_two() && v <= 8192 => align = Some(Some(v)),
                        _ => {
                            self.diags.error(
                                t.span,
                                "section alignment must be a power of two up to 8192",
                            );
                            return;
                        }
                    }
                }
                _ => {
                    self.diags
                        .error(t.span, format!("unknown COFF section attribute `{attr}`"));
                    return;
                }
            }
        }
        let n = self.interner.intern(&name);
        let existing = self.sections.iter().find(|s| s.name == n).map(|s| s.id);
        let flags = match (chosen, existing) {
            (Some(f), _) => Some(f),
            (None, Some(_)) => None,
            (None, None) => Some(match name.as_str() {
                ".data" => NASM_DATA,
                ".rdata" => NASM_RDATA,
                ".bss" => NASM_BSS,
                ".pdata" if win64 => NASM_PDATA,
                ".xdata" if win64 => NASM_XDATA,
                _ => NASM_TEXT,
            }),
        };
        let id = match existing {
            Some(id) => id,
            None => {
                let kind = if flags.is_some_and(|f| f & c::SCN_CNT_UNINITIALIZED_DATA != 0) {
                    SectionKind::Nobits
                } else {
                    SectionKind::Progbits
                };
                let core = crate::coff::section_flags(flags.unwrap_or(NASM_TEXT));
                self.get_or_create_section(n, kind, core, 1)
            }
        };
        // Whether the source has already asked for an alignment outright,
        // with `align=` or an `align` directive. Until it has, the section's
        // alignment is whatever its characteristics carry; from then on it is
        // the largest that was asked for, and the characteristics no longer
        // have a say. That is what keeps `section .s rdata align=2` at two
        // rather than at the eight `rdata` implies, and keeps a section an
        // `align 16` raised at sixteen when it is opened again with
        // `align=4`.
        let explicit = self.nasm.explicit_align.contains(&id);
        if let Some(f) = flags {
            let bits = (f & c::SCN_ALIGN_MASK) >> 20;
            let default = if bits == 0 { 1 } else { 1u64 << (bits - 1) };
            if !explicit {
                self.section_mut(id).align = default;
            }
            let info = self.coff_section_info(id);
            info.characteristics = f & !c::SCN_ALIGN_MASK;
        } else {
            // Named again without attributes: nothing changes, but the
            // section is still one the source named, which is what puts an
            // empty one in the object.
            self.coff_section_info(id);
        }
        // `align=0` asks for nothing and leaves both alone.
        if let Some(Some(a)) = align {
            let s = self.section_mut(id);
            s.align = if explicit { s.align.max(a) } else { a };
            self.nasm.explicit_align.insert(id);
        }
        let _ = span;
        self.set_section(id);
        self.nasm.absolute = None;
    }

    fn nasm_absolute(&mut self, args: &[Token], span: Span) {
        let toks = self.nasm_rewrite_locals(args);
        let here = self.nasm_here_for(&toks, span);
        let mut cur = Cursor::new(&toks);
        let mark = self.exprs.len();
        let Some(e) = self.parse_expr(&mut cur) else {
            return;
        };
        self.expect_end(&mut cur);
        self.nasm_bind_positional(mark, here);
        let Some(v) = self.eval_absolute(e, "`absolute` address") else {
            return;
        };
        self.nasm.absolute = Some(Absolute { base: v, here: v });
    }

    fn nasm_org(&mut self, args: &[Token], span: Span) {
        if self.options.relocatable {
            self.diags
                .error(span, "`org` is only meaningful in a flat binary");
            return;
        }
        let mut cur = Cursor::new(args);
        let Some(e) = self.parse_expr(&mut cur) else {
            return;
        };
        self.expect_end(&mut cur);
        let Some(v) = self.eval_absolute(e, "`org` address") else {
            return;
        };
        if let Some((prev, prev_span)) = self.nasm.org
            && prev != v as u64
        {
            self.diags.emit(
                crate::diag::Diagnostic::error(span, "program origin redefined")
                    .with_note(prev_span, "first defined here"),
            );
            return;
        }
        self.nasm.org = Some((v as u64, span));
        self.options.base_addr = v as u64;
    }

    /// `[global name:type visibility size]` and `[static ...]`.
    fn nasm_symbol_directive(&mut self, args: &[Token], span: Span, binding: Binding) {
        let Some((name, rest)) = self.nasm_symbol_name(args, span) else {
            return;
        };
        let id = self.symbols.intern(name, span);
        self.symbols.get_mut(id).binding = binding;
        let Some(rest) = rest else {
            return;
        };
        let mut i = 0;
        while i < rest.len() {
            let Some(n) = rest[i].ident() else {
                break;
            };
            let word = self.interner.get(n).to_ascii_lowercase();
            let sym = self.symbols.get_mut(id);
            match word.as_str() {
                "function" => sym.ty = SymType::Func,
                "data" | "object" => sym.ty = SymType::Object,
                "notype" => sym.ty = SymType::NoType,
                "hidden" => sym.visibility = Visibility::Hidden,
                "protected" => sym.visibility = Visibility::Protected,
                "internal" => sym.visibility = Visibility::Internal,
                "default" => sym.visibility = Visibility::Default,
                "weak" if binding == Binding::Global => sym.binding = Binding::Weak,
                "strong" if binding == Binding::Global => sym.binding = Binding::Global,
                _ => break,
            }
            i += 1;
        }
        if i < rest.len() {
            let toks = self.nasm_rewrite_locals(&rest[i..]);
            let mut cur = Cursor::new(&toks);
            let Some(e) = self.parse_expr(&mut cur) else {
                return;
            };
            self.expect_end(&mut cur);
            self.symbols.get_mut(id).size = Some(e);
        }
    }

    /// The symbol a symbol directive names, and the tokens after its `:`.
    fn nasm_symbol_name<'t>(
        &mut self,
        args: &'t [Token],
        span: Span,
    ) -> Option<(Name, Option<&'t [Token]>)> {
        let Some(n) = args.first().and_then(|t| t.ident()) else {
            self.diags.error(span, "expected a symbol name");
            return None;
        };
        let text = self.interner.get(n).to_string();
        let name = match self.nasm_label_name(&text) {
            Some(q) => self.interner.intern(&q),
            None => n,
        };
        match args.get(1) {
            None => Some((name, None)),
            Some(t) if t.is_punct(Punct::Colon) => Some((name, Some(&args[2..]))),
            Some(t) => {
                self.diags
                    .error(t.span, "expected `:` after the symbol name");
                None
            }
        }
    }

    fn nasm_extern(&mut self, args: &[Token], span: Span) {
        let Some((name, _)) = self.nasm_symbol_name(args, span) else {
            return;
        };
        let id = self.symbols.intern(name, span);
        if self.symbols.get(id).is_defined() {
            // NASM lets a symbol be declared extern and then defined; the
            // definition wins.
            return;
        }
        self.symbols.get_mut(id).binding = Binding::Global;
        self.nasm.externs.insert(name);
    }

    fn nasm_common(&mut self, args: &[Token], span: Span) {
        let Some(n) = args.first().and_then(|t| t.ident()) else {
            self.diags.error(span, "expected a symbol name");
            return;
        };
        let rest = &args[1..];
        let colon = rest.iter().position(|t| t.is_punct(Punct::Colon));
        let (size_toks, align_toks) = match colon {
            Some(c) => (&rest[..c], Some(&rest[c + 1..])),
            None => (rest, None),
        };
        let mut cur = Cursor::new(size_toks);
        let Some(e) = self.parse_expr(&mut cur) else {
            return;
        };
        let Some(size) = self.eval_absolute(e, "`common` size") else {
            return;
        };
        let mut align = 0;
        if let Some(a) = align_toks {
            let mut cur = Cursor::new(a);
            let Some(e) = self.parse_expr(&mut cur) else {
                return;
            };
            let Some(v) = self.eval_absolute(e, "`common` alignment") else {
                return;
            };
            if v <= 0 || !(v as u64).is_power_of_two() {
                self.diags.error(
                    span,
                    format!("alignment constraint `{v}` is not a power of two"),
                );
                return;
            }
            align = v as u64;
        }
        let id = self.symbols.intern(n, span);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Common {
            size: size.max(0) as u64,
            align,
        };
        sym.def_span = span;
        sym.binding = Binding::Global;
        self.symbols.mark_defined(id);
    }

    /// Reports symbols used but never defined nor declared `extern`, which
    /// NASM refuses rather than leaving to the linker.
    pub(crate) fn nasm_report_undefined(&mut self) {
        let mut missing = Vec::new();
        for (id, sym) in self.symbols.iter() {
            // `used` is only set later, when relocations are built, so it is
            // no guide here; every undefined user symbol that was not declared
            // `extern` is an error, as it is in NASM — a `global` naming an
            // undefined symbol included.
            if sym.is_defined()
                || sym.local_number.is_some()
                || self.nasm.externs.contains(&sym.name)
            {
                continue;
            }
            let raw = self.interner.get(sym.name);
            if raw.contains('\u{0}') || sym.ty == SymType::Section {
                continue;
            }
            // A special symbol after `wrt` is not a symbol.
            if raw.starts_with("..") && !raw.starts_with("..@") {
                continue;
            }
            missing.push((id, sym.first_use, raw.to_string()));
        }
        for (_, span, name) in missing {
            self.diags
                .error(span, format!("symbol `{name}` not defined"));
        }
    }
}

/// Pads `bytes` with zeros to a multiple of `width`.
fn pad_to(bytes: &mut Vec<u8>, width: usize) {
    let rem = bytes.len() % width;
    if rem != 0 {
        bytes.resize(bytes.len() + width - rem, 0);
    }
}

// The characteristics NASM's COFF writer gives each kind of section,
// alignment included (`outcoff.c`).
const NASM_TEXT: u32 = 0x6050_0020;
const NASM_DATA: u32 = 0xc030_0040;
const NASM_BSS: u32 = 0xc030_0080;
const NASM_RDATA: u32 = 0x4040_0040;
const NASM_PDATA: u32 = 0x4030_0040;
const NASM_XDATA: u32 = 0x4040_0040;
const NASM_INFO: u32 = 0x0010_0a00;

/// The type, flags and alignment NASM's ELF writer gives a section it
/// knows by name, and otherwise progbits, allocated, aligned to 1.
fn elf_section_defaults(name: &str) -> (SectionKind, SectionFlags, u64) {
    let f = |alloc, write, exec, tls| SectionFlags {
        alloc,
        write,
        exec,
        tls,
        ..Default::default()
    };
    match name {
        ".text" => (SectionKind::Progbits, f(true, false, true, false), 16),
        ".rodata" | ".lrodata" => (SectionKind::Progbits, f(true, false, false, false), 4),
        ".data" | ".ldata" => (SectionKind::Progbits, f(true, true, false, false), 4),
        ".bss" | ".lbss" => (SectionKind::Nobits, f(true, true, false, false), 4),
        ".tdata" => (SectionKind::Progbits, f(true, true, false, true), 4),
        ".tbss" => (SectionKind::Nobits, f(true, true, false, true), 4),
        ".comment" => (SectionKind::Progbits, f(false, false, false, false), 1),
        ".note" => (SectionKind::Note, f(false, false, false, false), 4),
        _ => (SectionKind::Progbits, f(true, false, false, false), 1),
    }
}

/// An x87 80-bit extended-precision value from a double, which is exact for
/// every value a double holds.
fn extended_bytes(v: f64) -> [u8; 10] {
    let bits = v.to_bits();
    let sign = (bits >> 63) as u16;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1 << 52) - 1);
    let (e, mant): (u16, u64) = if exp == 0 && frac == 0 {
        (0, 0)
    } else if exp == 0x7ff {
        (0x7fff, (1 << 63) | (frac << 11))
    } else if exp == 0 {
        // Subnormal: normalise.
        let shift = frac.leading_zeros() - 11;
        let m = frac << (shift + 11);
        ((1 - 1023 + 16383 - shift as i32) as u16, m)
    } else {
        ((exp - 1023 + 16383) as u16, (1 << 63) | (frac << 11))
    };
    let mut out = [0u8; 10];
    out[..8].copy_from_slice(&mant.to_le_bytes());
    out[8..].copy_from_slice(&((sign << 15) | e).to_le_bytes());
    out
}
