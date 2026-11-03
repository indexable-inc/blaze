//! Statement-level parsing.
//!
//! The parser is deliberately thin: it splits the token stream into statements
//! and recognises labels, directives, assignments and instructions. It does not
//! look inside operands — that grammar belongs to the architecture backend,
//! which receives the raw token tail.

use crate::cursor::Cursor;
use crate::diag::DiagBag;
use crate::intern::{Interner, Name};
use crate::lexer::{Dialect, LexConfig, Lexer, LitPool, Punct, TokKind, Token};
use crate::source::{FileId, SourceMap, Span};

/// The numeric local label that stands for CC-RX's `?:`: the largest number
/// a numeric label can have, which no real source writes.
pub const CCRX_TEMPORARY_LABEL: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub enum LabelDef {
    Named(Name, Span),
    /// A numeric local label such as `1:`.
    Numeric(u32, Span),
}

#[derive(Clone, Debug)]
pub enum Body {
    /// `.section .text` and friends.
    Directive { name: Name, span: Span },
    /// A machine instruction, handed to the current architecture.
    Insn { mnemonic: Name, span: Span },
    /// `sym = expr`, equivalent to `.set sym, expr`.
    Assign { name: Name, span: Span },
    /// `. = expr`: move the location counter.
    SetLocation { span: Span },
    /// Something that starts with none of the above.
    ///
    /// The parser classifies rather than judges: a line beginning with `\` is
    /// nonsense on its own but perfectly ordinary inside a macro body, and the
    /// parser cannot know which it is looking at. Reporting it is the
    /// assembler's job, once it knows whether the statement is going to be
    /// executed or captured.
    Unknown { span: Span },
}

#[derive(Clone, Debug)]
pub struct Statement {
    pub labels: Vec<LabelDef>,
    pub body: Option<Body>,
    /// A name written before a directive without a colon, in the CC-RL and
    /// CC-RH dialects: the section name of `CODE .CSEG`, the macro name of
    /// `ADMAC .MACRO`. It is not a label, so it defines nothing by itself;
    /// the directive decides what it means, or that it is not allowed.
    pub symbol: Option<(Name, Span)>,
    /// All tokens of the statement, excluding the terminator.
    pub toks: Vec<Token>,
    /// Index into `toks` of the first argument token.
    pub args: usize,
    pub span: Span,
}

impl Statement {
    pub fn arg_cursor(&self) -> Cursor<'_> {
        Cursor::new(&self.toks[self.args.min(self.toks.len())..])
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty() && self.body.is_none()
    }
}

/// Reads a file one statement at a time.
///
/// Between statements it holds a position in the file rather than a borrow
/// of the [`SourceMap`], so the assembler can carry out each statement before
/// the next one is lexed: an `.include` or a macro expansion adds a file to
/// the map, and an `.arch` switch changes the rules the rest of the file is
/// lexed by, through [`Parser::config_mut`].
pub struct Parser {
    file: FileId,
    /// Byte offset in the file at which the next statement is read.
    offset: usize,
    /// The rules the next statement is lexed by. Only `None` while a
    /// statement is being read, when the lexer holds them.
    config: Option<LexConfig>,
    /// An empty token buffer to read the next statement into.
    spare: Vec<Token>,
}

impl Parser {
    pub fn new(file: FileId, config: LexConfig) -> Parser {
        Parser {
            file,
            offset: 0,
            config: Some(config),
            spare: Vec::new(),
        }
    }

    /// The lexer configuration the rest of the file is read with. Changing it
    /// takes effect from the next statement on.
    pub fn config_mut(&mut self) -> &mut LexConfig {
        self.config.as_mut().expect("not reading a statement")
    }

    pub fn file(&self) -> FileId {
        self.file
    }

    /// Byte offset in the file just past the last statement read, and its
    /// terminator.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Moves the reading position to `offset`, which has to be where a line
    /// starts or the end of the file. The NASM preprocessor reads lines as
    /// text and uses this to step past the ones it has dealt with.
    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    /// Hands back a statement that has been dealt with, so the next one is
    /// read into its token buffer rather than a new allocation.
    pub fn recycle(&mut self, stmt: Statement) {
        let mut toks = stmt.toks;
        toks.clear();
        self.spare = toks;
    }

    pub fn dialect(&self) -> Dialect {
        self.config
            .as_ref()
            .expect("not reading a statement")
            .dialect
    }

    /// Reads the next non-empty statement, or `None` at end of file.
    ///
    /// The statement ends at its terminator, so a comment after it is read
    /// with it, by the rules it was written for.
    pub fn next_statement(
        &mut self,
        sm: &SourceMap,
        interner: &mut Interner,
        pool: &mut LitPool,
        diags: &mut DiagBag,
    ) -> Option<Statement> {
        let config = self.config.take().expect("not reading a statement");
        let mut lexer = Lexer::at(sm, self.file, config, self.offset);
        let mut toks = std::mem::take(&mut self.spare);
        loop {
            let t = lexer.next_token(interner, pool, diags);
            match t.kind {
                // The lexer does not move past the end of file, so the next
                // call finds it again and returns `None`.
                TokKind::Eof => break,
                // A blank line.
                TokKind::Eol if toks.is_empty() => {}
                TokKind::Eol => break,
                _ => toks.push(t),
            }
        }
        self.offset = lexer.offset();
        let mnemonic = lexer.config.mnemonic;
        let equates = lexer.config.equates;
        self.config = Some(lexer.config);
        if toks.is_empty() {
            self.spare = toks;
            return None;
        }
        let dialect = self.dialect();
        Some(
            Builder {
                dialect,
                mnemonic,
                equates,
            }
            .build(toks, interner, diags),
        )
    }
}

/// Splits a statement's tokens into labels and a body.
///
/// Exposed separately from [`Parser`] so that anything holding a bare token
/// vector — a macro expansion, say — can turn it into a statement without a
/// lexer. Without the lexer's configuration no backend's mnemonics are known,
/// so in the 8-bit dialect every first-column word that is not a directive is
/// a label.
#[allow(dead_code)]
pub fn build_statement(
    toks: Vec<Token>,
    dialect: Dialect,
    interner: &mut Interner,
    diags: &mut DiagBag,
) -> Statement {
    Builder {
        dialect,
        mnemonic: None,
        equates: &[],
    }
    .build(toks, interner, diags)
}

struct Builder {
    dialect: Dialect,
    /// See [`LexConfig::mnemonic`].
    mnemonic: Option<fn(&str) -> bool>,
    /// See [`LexConfig::equates`].
    equates: &'static [&'static str],
}

impl Builder {
    fn build(&self, toks: Vec<Token>, interner: &mut Interner, diags: &mut DiagBag) -> Statement {
        let stmt_span = toks
            .first()
            .zip(toks.last())
            .map(|(a, b)| a.span.to(b.span))
            .unwrap_or(Span::DUMMY);
        let span = stmt_span;

        let mut i = 0usize;
        let mut labels = Vec::new();

        // In Motorola source anything that starts in the first column is a
        // label, colon or not, and an instruction has to be indented to be one.
        // vasm and GNU as --mri both assemble `rts` written in column 0 to no
        // code at all: it defines a label called `rts`.
        if self.dialect == Dialect::Motorola
            && let Some(t) = toks.first()
            && let TokKind::Ident(n) = t.kind
            && !t.preceded_by_space
        {
            labels.push(LabelDef::Named(n, t.span));
            i = 1;
            if toks.get(i).map(|t| t.kind) == Some(TokKind::Punct(Punct::Colon)) {
                i += 1;
            }
        }

        // The 8-bit references split on this. vasm and AS take a first-column
        // word as a label, colon or not, like Motorola source; ca65 and GNU as
        // want the colon, and assemble an instruction written in the first
        // column. A word that names an instruction or a directive is taken as
        // one, so both kinds of source mean what they meant to their
        // assembler, unless a label is spelled like a mnemonic and has no
        // colon. `NAME = value` is an assignment either way.
        if self.dialect == Dialect::EightBit
            && let Some(t) = toks.first()
            && let TokKind::Ident(n) = t.kind
            && !t.preceded_by_space
            && !matches!(
                toks.get(1).map(|t| t.kind),
                Some(TokKind::Punct(Punct::Colon | Punct::Eq))
            )
            && !self.is_keyword(interner.get(n))
        {
            labels.push(LabelDef::Named(n, t.span));
            i = 1;
        }

        // Leading labels. A label is an identifier or a plain integer followed
        // by `:`; several may share a line with a statement.
        loop {
            match (toks.get(i).map(|t| t.kind), toks.get(i + 1).map(|t| t.kind)) {
                (Some(TokKind::Ident(n)), Some(TokKind::Punct(Punct::Colon))) => {
                    labels.push(LabelDef::Named(n, toks[i].span));
                    i += 2;
                    // `foo::` marks a global label in some dialects; accept and
                    // let the caller decide what it means.
                    if toks.get(i).map(|t| t.kind) == Some(TokKind::Punct(Punct::Colon)) {
                        i += 1;
                    }
                }
                (Some(TokKind::Int(v)), Some(TokKind::Punct(Punct::Colon))) => {
                    labels.push(LabelDef::Numeric(v as u32, toks[i].span));
                    i += 2;
                }
                // CC-RX's temporary label `?:`, which `?+` and `?-` refer to
                // (R20UT3248EJ0115 page 497): a numeric local label under a
                // number no source can write.
                (Some(TokKind::Punct(Punct::Question)), Some(TokKind::Punct(Punct::Colon)))
                    if self.dialect == Dialect::CcRx =>
                {
                    labels.push(LabelDef::Numeric(CCRX_TEMPORARY_LABEL, toks[i].span));
                    i += 2;
                }
                _ => break,
            }
        }

        // `NAME equ value`, the vendor spelling of `.set NAME, value`. The name
        // may already have been taken as a label — by a colon, or by starting
        // in the first column — in which case it is the name being defined
        // rather than a place. ca65 also writes `NAME := value`, which is the
        // same thing with `=` for the keyword.
        if self.dialect == Dialect::EightBit
            && toks.get(i).is_some_and(|t| t.is_punct(Punct::Eq))
            && let [LabelDef::Named(name, name_span)] = labels.as_slice()
        {
            let (name, span) = (*name, *name_span);
            return Statement {
                labels: Vec::new(),
                body: Some(Body::Assign { name, span }),
                symbol: None,
                args: i + 1,
                toks,
                span: stmt_span,
            };
        }
        if let Some(word) = toks.get(i).and_then(|t| t.ident())
            && self.is_equate_word(interner.get(word))
            && let [LabelDef::Named(name, name_span)] = labels.as_slice()
        {
            let (name, span) = (*name, *name_span);
            return Statement {
                labels: Vec::new(),
                body: Some(Body::Assign { name, span }),
                symbol: None,
                args: i + 1,
                toks,
                span: stmt_span,
            };
        }
        if labels.is_empty()
            && let (Some(name), Some(word)) = (
                toks.first().and_then(|t| t.ident()),
                toks.get(1).and_then(|t| t.ident()),
            )
            && self.is_equate_word(interner.get(word))
        {
            let span = toks[0].span;
            return Statement {
                labels,
                body: Some(Body::Assign { name, span }),
                symbol: None,
                args: 2,
                toks,
                span: stmt_span,
            };
        }

        let mut symbol = None;
        if self.dialect.is_cc() {
            // A control instruction is `$` and a word, spaces allowed around
            // the `$` (CC-RL §5.3, pages 539-555; CC-RH §5.3, pages 469-487).
            // It becomes a directive named `$word`.
            if let (Some(dollar), Some(word)) = (toks.get(i), toks.get(i + 1))
                && dollar.is_punct(Punct::Dollar)
                && let Some(n) = word.ident()
            {
                let name = interner.intern(&format!("${}", interner.get(n).to_ascii_lowercase()));
                return Statement {
                    labels,
                    body: Some(Body::Directive {
                        name,
                        span: dollar.span.to(word.span),
                    }),
                    symbol: None,
                    args: i + 2,
                    toks,
                    span: stmt_span,
                };
            }
            // `NAME .DIRECTIVE`: a symbol field without a colon, which only
            // the section and macro directives take (CC-RL §5.1.2 (3)(a),
            // page 427). Section names may start with a dot (`.text .CSEG`),
            // so the directive decides, not the name.
            if let (Some(name_tok), Some(dir)) = (toks.get(i), toks.get(i + 1))
                && let (Some(n), Some(d)) = (name_tok.ident(), dir.ident())
                && matches!(
                    interner.get(d).to_ascii_lowercase().as_str(),
                    ".cseg" | ".dseg" | ".bseg" | ".macro" | ".vector" | ".dbit"
                )
            {
                symbol = Some((n, name_tok.span));
                i += 1;
            }
        }
        // CC-RX names a macro, and a `.DEFINE` string, the same way
        // (R20UT3248EJ0115 pages 486 and 499).
        if self.dialect == Dialect::CcRx
            && let (Some(name_tok), Some(dir)) = (toks.get(i), toks.get(i + 1))
            && let (Some(n), Some(d)) = (name_tok.ident(), dir.ident())
            && matches!(
                interner.get(d).to_ascii_lowercase().as_str(),
                ".macro" | ".define"
            )
        {
            symbol = Some((n, name_tok.span));
            i += 1;
        }

        let body = self.classify(&toks, &mut i, interner, diags);
        Statement {
            labels,
            body,
            symbol,
            toks,
            args: i,
            span,
        }
    }

    /// The vendor keyword for defining a symbol, in dialects that have one.
    ///
    /// `set` is only taken where it cannot be an instruction: the Z80 has a
    /// `set 3, a` and is assembled in the NASM dialect, so NASM gets `equ`
    /// alone, which is also all NASM itself has.
    fn is_equate_word(&self, word: &str) -> bool {
        match self.dialect {
            Dialect::Gas => false,
            Dialect::Nasm => word.eq_ignore_ascii_case("equ"),
            Dialect::Motorola | Dialect::Renesas => {
                word.eq_ignore_ascii_case("equ") || word.eq_ignore_ascii_case("set")
            }
            // `NAME .EQU value` and `NAME .SET value` (CC-RL §5.2.3, pages
            // 502-504; CC-RH §5.2.3, pages 435-436).
            Dialect::CcRl | Dialect::CcRh => {
                word.eq_ignore_ascii_case(".equ") || word.eq_ignore_ascii_case(".set")
            }
            // CC-RX has `.EQU` alone (R20UT3248EJ0115 page 475).
            Dialect::CcRx => word.eq_ignore_ascii_case(".equ"),
            // `EQU` everywhere, AS's and Intel's `SET` and Zilog's `DEFL` for
            // a name that can be defined again, and ca65's `.set`. `SET` is
            // left alone where it is an instruction, as it is on the Z80.
            Dialect::EightBit => {
                let w = word.to_ascii_lowercase();
                match w.as_str() {
                    "equ" | ".equ" | "defl" | ".set" => true,
                    "set" => !self.mnemonic.is_some_and(|m| m("set")),
                    // The backend's own defining words, such as the MCS-51's
                    // `BIT` and `DATA`. An instruction of that name wins, as
                    // `SET` does on the Z80.
                    _ => {
                        self.equates.iter().any(|k| w == *k)
                            && !self.mnemonic.is_some_and(|m| m(&w))
                    }
                }
            }
        }
    }

    /// Whether a first-column word is an instruction or a directive, rather
    /// than a label, in the 8-bit dialect. A dotted word is a directive, as
    /// in ca65.
    fn is_keyword(&self, word: &str) -> bool {
        let lower = word.to_ascii_lowercase();
        lower.starts_with('.')
            || crate::dialect::lookup(self.dialect, &lower).is_some()
            || crate::dialect::block_keyword(self.dialect, &lower).is_some()
            || self.mnemonic.is_some_and(|m| m(&lower))
    }

    fn classify(
        &self,
        toks: &[Token],
        i: &mut usize,
        interner: &mut Interner,
        diags: &mut DiagBag,
    ) -> Option<Body> {
        let first = *toks.get(*i)?;

        // `. = expr` sets the location counter, and so does `* = expr` in the
        // 8-bit dialect, where vasm and ca65 write it.
        if (first.is_punct(Punct::Dot)
            || (first.is_punct(Punct::Star) && self.dialect == Dialect::EightBit))
            && toks.get(*i + 1).is_some_and(|t| t.is_punct(Punct::Eq))
        {
            *i += 2;
            return Some(Body::SetLocation { span: first.span });
        }

        // `{vex}`, `{evex}` and the other pseudo-prefixes GNU as and llvm-mc
        // take in front of an x86 instruction. They read as an instruction
        // named with its braces, which the backend treats as a prefix; any
        // other backend reports it as an instruction it does not know.
        if self.dialect == Dialect::Gas
            && first.is_punct(Punct::LBrace)
            && let (Some(word), Some(close)) = (toks.get(*i + 1), toks.get(*i + 2))
            && let TokKind::Ident(word) = word.kind
            && close.is_punct(Punct::RBrace)
        {
            let text = format!("{{{}}}", interner.get(word).to_ascii_lowercase());
            *i += 3;
            return Some(Body::Insn {
                mnemonic: interner.intern(&text),
                span: first.span.to(close.span),
            });
        }

        let TokKind::Ident(name) = first.kind else {
            *i = toks.len();
            let _ = &diags;
            return Some(Body::Unknown { span: first.span });
        };

        // `sym = expr` is an assignment, not an instruction called `sym`.
        if toks.get(*i + 1).is_some_and(|t| t.is_punct(Punct::Eq)) {
            *i += 2;
            return Some(Body::Assign {
                name,
                span: first.span,
            });
        }

        *i += 1;

        // Mnemonics and directives are case-insensitive; symbol names are not.
        let (is_directive, folded) = {
            let text = interner.get(name);
            let is_directive = match self.dialect {
                // GAS spells every directive with a leading dot. A bare `.` is
                // the location counter and was handled above.
                Dialect::Gas => text.starts_with('.') && text.len() > 1,
                // NASM directives are bare words; the assembler resolves them
                // against its directive table and falls back to an instruction.
                Dialect::Nasm => false,
                // Bare words are resolved the same way. A dotted spelling is a
                // directive too: Renesas's newer assemblers write `.DB` and
                // `.CSEG`, and a Motorola `.local` label never reaches here,
                // because a first-column word has already been taken as one.
                Dialect::Motorola | Dialect::Renesas | Dialect::EightBit => {
                    text.starts_with('.') && text.len() > 1
                }
                // Every CC-RL and CC-RH directive is dotted and every bare word
                // is an instruction or a macro call (CC-RL Table 5.13, page
                // 484).
                Dialect::CcRl | Dialect::CcRh | Dialect::CcRx => {
                    text.starts_with('.') && text.len() > 1
                }
            };
            let folded = text
                .bytes()
                .any(|b| b.is_ascii_uppercase())
                .then(|| text.to_ascii_lowercase());
            (is_directive, folded)
        };
        let lowered = match folded {
            Some(t) => interner.intern(&t),
            None => name,
        };
        if is_directive {
            Some(Body::Directive {
                name: lowered,
                span: first.span,
            })
        } else {
            Some(Body::Insn {
                mnemonic: lowered,
                span: first.span,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct H {
        sm: SourceMap,
        interner: Interner,
        pool: LitPool,
        diags: DiagBag,
    }

    fn parse(src: &str) -> (Vec<Statement>, H) {
        let mut h = H {
            sm: SourceMap::new(),
            interner: Interner::new(),
            pool: LitPool::new(),
            diags: DiagBag::new(),
        };
        let f = h.sm.add("t.s", src);
        let mut out = Vec::new();
        {
            let mut p = Parser::new(f, LexConfig::for_dialect(Dialect::Gas));
            while let Some(s) = p.next_statement(&h.sm, &mut h.interner, &mut h.pool, &mut h.diags)
            {
                out.push(s);
            }
        }
        (out, h)
    }

    #[test]
    fn splits_labels_from_instructions() {
        let (st, h) = parse("foo: bar: movq %rax, %rbx\n");
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].labels.len(), 2);
        let Some(Body::Insn { mnemonic, .. }) = st[0].body else {
            panic!("{:?}", st[0].body)
        };
        assert_eq!(h.interner.get(mnemonic), "movq");
        assert_eq!(st[0].arg_cursor().rest().len(), 5);
    }

    #[test]
    fn recognises_directives_and_numeric_labels() {
        let (st, h) = parse("1: .byte 1, 2\n");
        assert!(matches!(st[0].labels[0], LabelDef::Numeric(1, _)));
        let Some(Body::Directive { name, .. }) = st[0].body else {
            panic!()
        };
        assert_eq!(h.interner.get(name), ".byte");
    }

    #[test]
    fn assignment_beats_instruction() {
        let (st, h) = parse("count = 4 * 2\n");
        let Some(Body::Assign { name, .. }) = st[0].body else {
            panic!("{:?}", st[0].body)
        };
        assert_eq!(h.interner.get(name), "count");
        assert_eq!(st[0].arg_cursor().rest().len(), 3);
    }

    #[test]
    fn location_counter_assignment() {
        let (st, _) = parse(". = . + 16\n");
        assert!(matches!(st[0].body, Some(Body::SetLocation { .. })));
    }

    #[test]
    fn semicolons_separate_statements_in_gas() {
        let (st, _) = parse("nop; nop; nop\n");
        assert_eq!(st.len(), 3);
    }

    #[test]
    fn label_only_lines_and_blank_lines() {
        let (st, _) = parse("\n\nfoo:\n\n  nop\n");
        assert_eq!(st.len(), 2);
        assert!(st[0].body.is_none());
        assert_eq!(st[0].labels.len(), 1);
    }

    #[test]
    fn mnemonics_fold_case_but_labels_do_not() {
        let (st, h) = parse("Foo: NOP\n");
        let Some(Body::Insn { mnemonic, .. }) = st[0].body else {
            panic!()
        };
        assert_eq!(h.interner.get(mnemonic), "nop");
        let LabelDef::Named(n, _) = st[0].labels[0] else {
            panic!()
        };
        assert_eq!(h.interner.get(n), "Foo");
    }

    #[test]
    fn last_line_without_newline_still_parses() {
        let (st, _) = parse("nop");
        assert_eq!(st.len(), 1);
    }
}
