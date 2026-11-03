//! Tokenizer.
//!
//! The lexer is pull-based and its [`LexConfig`] is mutable between tokens.
//! That matters because directives such as `.arch` change how the *rest* of
//! the file is spelled: `#` starts a comment on x86 but an immediate on m68k,
//! `|` a comment on m68k but an operator elsewhere. A batch tokenizer would
//! have to guess; a pull lexer simply asks the config again for every token,
//! and the crate's parser reads a file one statement at a time so the
//! directive has run before the next statement is lexed.

use crate::diag::{DiagBag, Diagnostic};
use crate::intern::{Interner, Name};
use crate::source::{FileId, SourceMap, Span};

/// Overall source-language flavour. Controls lexing and the directive set.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum Dialect {
    /// GNU as: `#` and `//` comments, `;` separates statements, `.directives`.
    #[default]
    Gas,
    /// NASM: `;` comments, no statement separator, bare `directives`, a
    /// `%` preprocessor, and three kinds of quotes. Its directives and
    /// preprocessor live in the crate-private `nasm` module.
    Nasm,
    /// Motorola, as spoken by vasm, Devpac and ASM-One and understood by GNU
    /// as in `--mri` mode: `$7fff` hex, `%1010` binary, `@17` octal, `;`
    /// comments and `*` comments in the first column, `dc.w`-style directives
    /// without a dot, and a label is anything that starts in the first column.
    Motorola,
    /// Renesas vendor assemblers (CA78K0 and its successors): `;` comments,
    /// `0FFH` radix suffixes, and bare `CSEG`/`DB`/`ORG` directives. `$` is not
    /// a hex prefix here: it is the location counter, and on 78K0 an
    /// operand's relative-addressing sigil.
    Renesas,
    /// Renesas CC-RL, the assembler of the RL78 compiler package: dotted
    /// `.DB`/`.CSEG` directives, `$IF`-style control instructions, both
    /// `0x10` and `10H` numbers, and C escapes in quoted strings. See
    /// the crate's `dialect` module for the manual it follows.
    CcRl,
    /// Renesas CC-RH, the assembler of the RH850 compiler package. The same
    /// language family as CC-RL, with prefix-only numbers, a different
    /// operator precedence and `!` as the bitwise NOT.
    CcRh,
    /// Renesas CC-RX, the assembler of the RX compiler package: suffix-only
    /// numbers, `$` as the location counter, `.SECTION P,CODE`-style
    /// directives and `?:` temporary labels.
    CcRx,
    /// The conventional syntax of the 8-bit microprocessors, as cc65's ca65
    /// reads 6502 source, GNU as and vasm read Zilog Z80 source, and the
    /// Macro Assembler AS reads Intel 8080 source: `$12`, `12H` and `%1010`
    /// numbers, `;` comments, `$` and `*` for the location counter, `<` and
    /// `>` for an address's low and high byte, `DB`/`DEFB`/`.byte` data, and
    /// `ORG` to say where the code is loaded. See the crate's `dialect`
    /// module for how the references differ and which one each rule follows.
    EightBit,
}

impl Dialect {
    pub fn from_name(name: &str) -> Option<Dialect> {
        Some(match name.to_ascii_lowercase().as_str() {
            "gas" | "gnu" | "att" => Dialect::Gas,
            "nasm" => Dialect::Nasm,
            "motorola" | "mot" | "vasm" | "devpac" | "mri" => Dialect::Motorola,
            "renesas" | "ca78k0" | "nec" => Dialect::Renesas,
            "ccrl" | "cc-rl" => Dialect::CcRl,
            "ccrh" | "cc-rh" => Dialect::CcRh,
            "ccrx" | "cc-rx" => Dialect::CcRx,
            "8bit" | "eightbit" | "ca65" | "zilog" | "oldstyle" => Dialect::EightBit,
            _ => return None,
        })
    }

    /// Whether directives are spelled without a leading dot, so a bare word
    /// has to be looked up before it can be called an instruction.
    pub fn dotless_directives(self) -> bool {
        matches!(
            self,
            Dialect::Nasm | Dialect::Motorola | Dialect::Renesas | Dialect::EightBit
        )
    }

    /// The CC-RL/CC-RH family, which shares its directives, control
    /// instructions and macro language.
    pub fn is_cc(self) -> bool {
        matches!(self, Dialect::CcRl | Dialect::CcRh)
    }

    /// Renesas's current assemblers, CC-RL, CC-RH and CC-RX: every directive
    /// is dotted, a bare word is an instruction or a macro call, and a macro
    /// parameter is a plain word rather than `\name`.
    pub fn renesas_cc(self) -> bool {
        matches!(self, Dialect::CcRl | Dialect::CcRh | Dialect::CcRx)
    }

    /// `$` on its own is the location counter (and `$$` the section start).
    /// CC-RX calls it the location symbol (R20UT3248EJ0115 Table 5.1, page
    /// 453).
    pub fn dollar_is_here(self) -> bool {
        matches!(
            self,
            Dialect::Nasm | Dialect::Renesas | Dialect::CcRx | Dialect::EightBit
        )
    }

    /// Quotes inside a string are written twice (`'it''s'`). Devpac, vasm,
    /// GNU as `--mri` and the RA78K0 manual agree, and CC-RL says the same of
    /// a double quote (R20UT3123EJ0115 §5.1.2 (2)(c), page 426).
    pub fn doubled_quotes(self) -> bool {
        matches!(self, Dialect::Motorola | Dialect::Renesas | Dialect::CcRl)
    }

    /// A backslash starts a C escape sequence in a quoted literal. The older
    /// vendor syntaxes treat it as an ordinary character; CC-RL and CC-RH list
    /// `\n`, `\xhh` and the rest (CC-RL Table 5.3, page 424; CC-RH Table 5.2,
    /// R20UT3516EJ0113 page 382). CC-RX's manual describes none, so its
    /// strings are taken as written. NASM reads escapes only in backquoted
    /// strings, which its own string lexer handles.
    ///
    /// ca65 (without `.feature string_escapes`) and GNU as for the Z80 take a
    /// backslash as written too.
    pub fn backslash_escapes(self) -> bool {
        !matches!(
            self,
            Dialect::Motorola
                | Dialect::Renesas
                | Dialect::CcRx
                | Dialect::EightBit
                | Dialect::Nasm
        )
    }

    /// `*` in operand position is the location counter, as in `dc.l *`. It is
    /// still multiplication between two operands. ca65 and vasm spell the
    /// location counter this way for the 6502.
    pub fn star_is_here(self) -> bool {
        matches!(self, Dialect::Motorola | Dialect::EightBit)
    }
}

/// Lexical rules in force for the next token.
/// Not API.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct LexConfig {
    pub dialect: Dialect,
    /// Strings that begin a comment running to end of line, wherever they
    /// appear.
    pub line_comment: Vec<&'static str>,
    /// Strings that begin a comment only at the start of a line.
    ///
    /// This is how GAS reconciles `#` being a comment everywhere with `#` being
    /// the immediate prefix on ARM, AArch64 and SPARC: on those targets `#` is
    /// a comment only in the first column (where it is also what C
    /// preprocessor line markers look like), and `mov r0, #1` keeps its `#`.
    pub line_start_comment: Vec<&'static str>,
    /// Whether `/* ... */` is a comment.
    pub block_comment: bool,
    /// Characters that terminate a statement like a newline does.
    pub stmt_sep: Vec<char>,
    /// Accept trailing radix letters: `0ffh`, `1010b`, `17o`, `99d`.
    pub radix_suffix: bool,
    /// Accept `<digits>f` / `<digits>b` as forward/backward local-label refs.
    /// Mutually exclusive with `radix_suffix` for the `b` case.
    pub local_label_refs: bool,
    /// A character literal may hold several characters, packed big-endian, and
    /// requires a closing quote (NASM). When false, `'a` is one character with
    /// an optional closing quote (GAS).
    pub char_multi: bool,
    /// A bare leading `0` introduces an octal literal (GAS).
    pub octal_leading_zero: bool,
    /// Single-character radix prefixes, such as Motorola's `$7fff`.
    ///
    /// A prefix only counts when a digit of its radix follows, which is what
    /// keeps `%` a register sigil or modulo operator and `$` a punctuation
    /// mark wherever they are not starting a number.
    pub number_prefixes: Vec<(char, u32)>,
    /// `@` may start or continue an identifier, as the CC-RL and CC-RH symbol
    /// rules allow (CC-RL §5.1.2 (3)(b), page 428; CC-RH §5.1.12, page 423).
    pub at_in_idents: bool,
    /// `$` may continue an identifier, as GNU as's default character table
    /// has it. AVR's port takes it out (`LEX_DOLLAR 0`), since there `$`
    /// separates statements.
    pub dollar_in_idents: bool,
    /// `AF'`, the Z80's alternate register pair, is a name: a quote straight
    /// after `AF` is its prime rather than the start of a character literal.
    pub primed_af: bool,
    /// Whether `.` separates a bit number from the byte it belongs to, as it
    /// does on the MCS-51; see
    /// [`crate::arch::Architecture::bit_addressing`]. A `.` is then never
    /// part of an identifier, and starts one only before a letter, so `P1.3`
    /// and `20H.3` come out as three tokens while ca65's `.byte` stays one.
    pub bit_dot: bool,
    /// Extra words that define a symbol where a label would go, in a dialect
    /// where a bare word can be a directive; see
    /// [`crate::arch::Architecture::equates`].
    pub equates: &'static [&'static str],
    /// Recognises the active backend's mnemonics, in a dialect where a word
    /// in the first column is a label unless it is an instruction or a
    /// directive; see [`crate::arch::Architecture::mnemonics`].
    pub mnemonic: Option<fn(&str) -> bool>,
}

impl LexConfig {
    pub fn for_dialect(d: Dialect) -> LexConfig {
        match d {
            Dialect::Gas => LexConfig {
                dialect: d,
                line_comment: vec!["#", "//"],
                line_start_comment: vec![],
                block_comment: true,
                stmt_sep: vec![';'],
                radix_suffix: false,
                local_label_refs: true,
                char_multi: false,
                octal_leading_zero: true,
                number_prefixes: vec![],
                at_in_idents: false,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            // Numbers, names, strings and NASM's own operators are read by
            // `Lexer::lex_nasm`, so only the comment rules here matter.
            Dialect::Nasm => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec![],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: true,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: false,
                number_prefixes: vec![],
                at_in_idents: false,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            // Checked against vasm and GNU as --mri, which agree on every rule
            // but one: a `#` where a line begins is a comment to GNU as, whose
            // m68k port lists `#` and `*` as its line comment characters in
            // either mode, and an error to vasm. Nothing else can start a
            // Motorola line with a `#`, since the immediate prefix belongs to
            // an operand, so the reading that assembles more source wins.
            Dialect::Motorola => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec!["*", "#"],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: false,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: false,
                number_prefixes: vec![('$', 16), ('%', 2), ('@', 8)],
                at_in_idents: false,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            Dialect::Renesas => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec![],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: true,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: false,
                number_prefixes: vec![],
                at_in_idents: false,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            // CC-RL: `;` comments, and `#` ones at the start of a line
            // (§5.1.2 (6), page 429). A number takes a `0x`/`0b` prefix or an
            // `H`/`B`/`O` suffix, and a leading `0` makes it octal (§5.1.2
            // (2)(a), page 426). The manual has a program pick one notation
            // with `-base_number`; rsasm reads both, so `0FFH` works and
            // `010` is the default prefix notation's eight.
            Dialect::CcRl => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec!["#"],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: true,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: true,
                number_prefixes: vec![],
                at_in_idents: true,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            // CC-RH: the same comments (§5.1.1 (5), page 383, and the `#` row
            // of Table 5.1, page 379), and prefix notation only (§5.1.1
            // (4)(a), page 381).
            Dialect::CcRh => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec!["#"],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: false,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: true,
                number_prefixes: vec![],
                at_in_idents: true,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            // CC-RX: `;` comments only, since `#` is the immediate sigil
            // (R20UT3248EJ0115 §5.1.7, page 463), and numbers with a `B`, `O`
            // or `H` suffix or none, a leading zero included (§5.1.5 (1),
            // pages 455-456).
            Dialect::CcRx => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec![],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: true,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: false,
                number_prefixes: vec![],
                at_in_idents: false,
                dollar_in_idents: true,
                primed_af: false,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
            // The 8-bit references agree on `;` comments, no statement
            // separator, and `$` and `%` prefixes. ca65 has no radix suffixes,
            // but GNU as for the Z80, vasm and AS all read `0FFH`; a `0B00H`
            // is hex, as it is to all three. Only GNU as has `1b` local label
            // references, which the others would read as binary, as this does.
            Dialect::EightBit => LexConfig {
                dialect: d,
                line_comment: vec![";"],
                line_start_comment: vec![],
                block_comment: false,
                stmt_sep: vec![],
                radix_suffix: true,
                local_label_refs: false,
                char_multi: true,
                octal_leading_zero: false,
                number_prefixes: vec![('$', 16), ('%', 2)],
                at_in_idents: false,
                dollar_in_idents: true,
                primed_af: true,
                bit_dot: false,
                equates: &[],
                mnemonic: None,
            },
        }
    }
}

impl Default for LexConfig {
    fn default() -> LexConfig {
        LexConfig::for_dialect(Dialect::default())
    }
}

/// Direction of a numeric local-label reference (`1f` / `1b`).
/// Not API.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LocalDir {
    Forward,
    Backward,
}

/// Not API.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Punct {
    Comma,
    Colon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Tilde,
    Bang,
    Lt,
    Gt,
    Eq,
    At,
    Dollar,
    Hash,
    /// A lone `.`: the location counter in GAS.
    Dot,
    Question,
    Backslash,
    Shl,
    Shr,
    EqEq,
    Ne,
    Le,
    Ge,
    AndAnd,
    OrOr,
    /// NASM's signed division, `//`.
    SlashSlash,
    /// NASM's signed remainder, `%%`.
    PercentPercent,
    /// NASM's arithmetic right shift, `>>>`.
    Sar,
    /// NASM's logical exclusive or, `^^`.
    CaretCaret,
    /// NASM's signed three-way comparison, `<=>`.
    Spaceship,
}

impl Punct {
    #[rustfmt::skip]
    pub fn as_str(self) -> &'static str {
        use Punct::*;
        match self {
            Comma => ",", Colon => ":", LParen => "(", RParen => ")",
            LBracket => "[", RBracket => "]", LBrace => "{", RBrace => "}",
            Plus => "+", Minus => "-", Star => "*", Slash => "/",
            Percent => "%", Amp => "&", Pipe => "|", Caret => "^",
            Tilde => "~", Bang => "!", Lt => "<", Gt => ">", Eq => "=",
            At => "@", Dollar => "$", Hash => "#", Dot => ".",
            Question => "?", Backslash => "\\",
            Shl => "<<", Shr => ">>", EqEq => "==", Ne => "!=",
            Le => "<=", Ge => ">=", AndAnd => "&&", OrOr => "||",
            SlashSlash => "//", PercentPercent => "%%", Sar => ">>>",
            CaretCaret => "^^", Spaceship => "<=>",
        }
    }
}

/// Byte strings from string literals, shared across every file in a run so a
/// `TokKind::Str` index stays valid after the lexer that produced it is gone.
/// Not API.
#[doc(hidden)]
#[derive(Default)]
pub struct LitPool {
    strings: Vec<Vec<u8>>,
}

impl LitPool {
    pub fn new() -> LitPool {
        LitPool::default()
    }

    pub fn add(&mut self, bytes: Vec<u8>) -> u32 {
        let i = self.strings.len() as u32;
        self.strings.push(bytes);
        i
    }

    pub fn get(&self, idx: u32) -> &[u8] {
        &self.strings[idx as usize]
    }
}

/// Not API.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TokKind {
    Eof,
    /// End of statement: a newline or a dialect statement separator.
    Eol,
    Ident(Name),
    Int(u64),
    /// Index into the lexer's string pool.
    Str(u32),
    Punct(Punct),
    /// `1f` / `2b` style reference to a numeric local label.
    LocalRef(u32, LocalDir),
    /// A run that starts like a number but is not one, such as `1to16` or
    /// `08`. Carries the text as written.
    ///
    /// The lexer does not report it, because it cannot tell whether it is an
    /// error: `1to16` is a malformed literal in an expression and a perfectly
    /// good broadcast count inside an AVX-512 `{1to16}` decorator. Whoever
    /// consumes the token knows which; the expression parser reports it with
    /// [`explain_bad_number`].
    BadNumber(Name),
}

/// The radix of a literal written with a trailing radix letter (`0ffh`,
/// `1010b`, `17o`, `99d`), if every character before the letter is a digit of
/// that radix.
fn suffix_radix(run: &str) -> Option<u32> {
    let last = run.as_bytes().last().copied()?;
    let radix = match last | 0x20 {
        b'h' => 16,
        b'b' | b'y' => 2,
        b'o' | b'q' => 8,
        b'd' | b't' => 10,
        _ => return None,
    };
    let body = &run[..run.len() - 1];
    (!body.is_empty() && body.chars().all(|c| c == '_' || c.is_digit(radix))).then_some(radix)
}

/// The diagnostic for a [`TokKind::BadNumber`] read where a number was
/// expected: names the first character that is not a digit, and the base.
/// Not API.
#[doc(hidden)]
pub fn explain_bad_number(text: &str) -> String {
    let (radix, digits) = match text.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => (16, &text[2..]),
        Some("0b") => (2, &text[2..]),
        Some("0o") => (8, &text[2..]),
        _ if text.len() > 1 && text.starts_with('0') => (8, &text[1..]),
        _ => (10, text),
    };
    match digits
        .chars()
        .find(|c| *c != '_' && c.to_digit(radix).is_none())
    {
        Some(c) => format!("invalid digit `{c}` for base-{radix} literal `{text}`"),
        // Every digit is valid, so the number is too big.
        None if !digits.is_empty() => "integer literal out of range for 64 bits".into(),
        None => format!("invalid integer literal `{text}`"),
    }
}

/// Not API.
#[doc(hidden)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Token {
    pub kind: TokKind,
    pub span: Span,
    /// True when whitespace immediately precedes this token. Some syntaxes
    /// need it (NASM distinguishes `foo:` at column 0, AT&T needs to split a
    /// mnemonic from its operands).
    pub preceded_by_space: bool,
}

impl Token {
    pub fn is_eol(&self) -> bool {
        matches!(self.kind, TokKind::Eol | TokKind::Eof)
    }

    pub fn is_punct(&self, p: Punct) -> bool {
        self.kind == TokKind::Punct(p)
    }

    pub fn ident(&self) -> Option<Name> {
        match self.kind {
            TokKind::Ident(n) => Some(n),
            _ => None,
        }
    }
}

/// Not API.
#[doc(hidden)]
pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    /// Global position of `src[0]`.
    base: u32,
    /// Byte offset within `src`.
    pos: usize,
    pub config: LexConfig,
    /// True until the first token of a physical line, so line-start comments
    /// can be told apart from the same characters later in the line.
    at_line_start: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(sm: &'a SourceMap, file: FileId, config: LexConfig) -> Lexer<'a> {
        Lexer::at(sm, file, config, 0)
    }

    /// A lexer that starts `offset` bytes into `file`, in the state a lexer
    /// that had read up to there would be in. `offset` has to be a place a
    /// token or trivia could start, such as just past an end of line.
    pub fn at(sm: &'a SourceMap, file: FileId, config: LexConfig, offset: usize) -> Lexer<'a> {
        let f = sm.file(file);
        Lexer {
            src: &f.src,
            bytes: f.src.as_bytes(),
            base: f.start,
            pos: offset,
            config,
            // What `next_token` would have set: a newline starts a line, a
            // `;` does not.
            at_line_start: offset == 0 || f.src[..offset].ends_with('\n'),
        }
    }

    /// Byte offset within the file of the next character to be read.
    pub fn offset(&self) -> usize {
        self.pos
    }

    /// Current global position.
    fn gpos(&self) -> u32 {
        self.base + self.pos as u32
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(self.base + start as u32, self.gpos())
    }

    fn peek(&self) -> u8 {
        self.bytes.get(self.pos).copied().unwrap_or(0)
    }

    fn peek_at(&self, n: usize) -> u8 {
        self.bytes.get(self.pos + n).copied().unwrap_or(0)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// Length in bytes of the character at the cursor. `pos` is always on a
    /// character boundary, so this is well defined.
    fn char_len(&self) -> usize {
        self.src[self.pos..]
            .chars()
            .next()
            .map_or(1, char::len_utf8)
    }

    fn starts_with(&self, s: &str) -> bool {
        self.bytes[self.pos.min(self.bytes.len())..].starts_with(s.as_bytes())
    }

    /// Skips spaces, comments and escaped newlines. Returns true if anything
    /// was skipped (so the next token knows it is space-preceded).
    fn skip_trivia(&mut self, diags: &mut DiagBag) -> bool {
        let start = self.pos;
        loop {
            let c = self.peek();
            match c {
                b' ' | b'\t' | b'\r' | 0x0b | 0x0c => {
                    self.pos += 1;
                }
                b'\\' if matches!(self.peek_at(1), b'\n') => {
                    self.pos += 2;
                }
                b'\\' if self.peek_at(1) == b'\r' && self.peek_at(2) == b'\n' => {
                    self.pos += 3;
                }
                _ => {
                    if self.config.block_comment && self.starts_with("/*") {
                        let open = self.pos;
                        self.pos += 2;
                        loop {
                            if self.at_end() {
                                diags.emit(Diagnostic::error(
                                    self.span_from(open),
                                    "unterminated block comment",
                                ));
                                break;
                            }
                            if self.starts_with("*/") {
                                self.pos += 2;
                                break;
                            }
                            self.pos += 1;
                        }
                        continue;
                    }
                    let mut matched = false;
                    let rest = &self.bytes[self.pos.min(self.bytes.len())..];
                    if self.at_line_start
                        && self
                            .config
                            .line_start_comment
                            .iter()
                            .any(|lc| rest.starts_with(lc.as_bytes()))
                    {
                        while !self.at_end() && self.peek() != b'\n' {
                            self.pos += 1;
                        }
                        continue;
                    }
                    for lc in &self.config.line_comment {
                        if self.bytes[self.pos.min(self.bytes.len())..].starts_with(lc.as_bytes()) {
                            while !self.at_end() && self.peek() != b'\n' {
                                self.pos += 1;
                            }
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        break;
                    }
                }
            }
        }
        self.pos != start
    }

    pub fn next_token(
        &mut self,
        interner: &mut Interner,
        pool: &mut LitPool,
        diags: &mut DiagBag,
    ) -> Token {
        let spaced = self.skip_trivia(diags);
        let token = self.lex_one(spaced, interner, pool, diags);
        // Only a physical newline starts a line. A `;` statement separator
        // does not, which is what GAS does too.
        self.at_line_start = token.kind == TokKind::Eol && self.src[..self.pos].ends_with('\n');
        token
    }

    fn lex_one(
        &mut self,
        spaced: bool,
        interner: &mut Interner,
        pool: &mut LitPool,
        diags: &mut DiagBag,
    ) -> Token {
        let start = self.pos;
        let mk = |k: TokKind, this: &Self| Token {
            kind: k,
            span: this.span_from(start),
            preceded_by_space: spaced,
        };

        if self.at_end() {
            return mk(TokKind::Eof, self);
        }

        let c = self.peek();

        if c == b'\n' {
            self.pos += 1;
            return mk(TokKind::Eol, self);
        }
        if self.config.stmt_sep.contains(&(c as char)) {
            self.pos += 1;
            return mk(TokKind::Eol, self);
        }

        if self.config.dialect == Dialect::Nasm {
            if let Some(tok) = self.lex_nasm(start, spaced, interner, pool, diags) {
                return tok;
            }
        } else if c.is_ascii_digit() {
            return self.lex_number(start, spaced, interner, diags);
        }

        // `$7fff`, `%1010`, `@17`, where the dialect has them.
        if let Some(tok) = self.lex_prefixed_number(start, spaced, diags) {
            return tok;
        }

        let at = self.config.at_in_idents;
        let dollar = self.config.dollar_in_idents;
        let dots = !self.config.bit_dot;
        let cont = |b: u8| {
            (is_ident_cont(b) && (dots || b != b'.') && (dollar || b != b'$')) || (at && b == b'@')
        };
        // A leading `.` introduces a directive name. Where `.` is the bit
        // separator it may only do so before a letter, so `20H.3` splits.
        let dot_starts = c == b'.'
            && if dots {
                cont(self.peek_at(1))
            } else {
                is_ident_start(self.peek_at(1))
            };
        if is_ident_start(c) || (at && c == b'@') || dot_starts {
            // The leading `.` is taken here, since where it is the bit
            // separator it does not continue a name.
            if c == b'.' {
                self.pos += 1;
            }
            // Identifiers may contain non-ASCII characters, so advance by
            // whole characters and never leave `pos` inside one.
            while !self.at_end() && cont(self.peek()) {
                self.pos += self.char_len();
            }
            // The Z80's alternate register pair, `AF'`: a quote straight after
            // a name cannot open a character literal, so it is the prime.
            if self.config.primed_af
                && self.peek() == b'\''
                && self.src[start..self.pos].eq_ignore_ascii_case("af")
            {
                self.pos += 1;
            }
            let text = &self.src[start..self.pos];
            let name = interner.intern(text);
            return mk(TokKind::Ident(name), self);
        }

        if c == b'"' {
            return self.lex_string(start, spaced, pool, diags);
        }
        if c == b'\'' {
            return self.lex_char(start, spaced, diags);
        }

        self.lex_punct(start, spaced, diags)
    }

    /// The NASM tokens that differ from the other dialects: numbers, names,
    /// strings and the operators only NASM has. `None`, with nothing
    /// consumed, for anything else.
    ///
    /// As NASM 2.16.03 reads them: `$` before a digit is a hex prefix and
    /// before a name marks the name as a symbol rather than a keyword
    /// (`$eax`); a name starts with a letter, `.`, `_` or `?` and goes on
    /// with those, digits, `$`, `#`, `@` and `~`; `'` and `"` quote text as
    /// written, with no escapes and no doubled quotes, while `` ` `` reads C
    /// escapes. A quoted string is always a [`TokKind::Str`], even where it
    /// stands for a number, since only the consumer knows which it is.
    fn lex_nasm(
        &mut self,
        start: usize,
        spaced: bool,
        interner: &mut Interner,
        pool: &mut LitPool,
        diags: &mut DiagBag,
    ) -> Option<Token> {
        let c = self.peek();
        let mk = |kind, this: &Self| Token {
            kind,
            span: this.span_from(start),
            preceded_by_space: spaced,
        };
        if c.is_ascii_digit() || (c == b'$' && self.peek_at(1).is_ascii_digit()) {
            let kind = self.lex_nasm_number(interner);
            return Some(mk(kind, self));
        }
        if c == b'$' && nasm_name_start(self.peek_at(1)) {
            self.pos += 1;
            let from = self.pos;
            while !self.at_end() && nasm_name_cont(self.peek()) {
                self.pos += self.char_len();
            }
            let name = interner.intern(&self.src[from..self.pos]);
            return Some(mk(TokKind::Ident(name), self));
        }
        // A lone `.` is not a name, though `.x` and `..@x` are.
        if nasm_name_start(c) && (c != b'.' || nasm_name_cont(self.peek_at(1))) {
            while !self.at_end() && nasm_name_cont(self.peek()) {
                self.pos += self.char_len();
            }
            let name = interner.intern(&self.src[start..self.pos]);
            return Some(mk(TokKind::Ident(name), self));
        }
        if matches!(c, b'\'' | b'"' | b'`') {
            self.pos += 1;
            let mut buf = Vec::new();
            loop {
                if self.at_end() || self.peek() == b'\n' {
                    diags.error(self.span_from(start), "unterminated string literal");
                    break;
                }
                let d = self.peek();
                if d == c {
                    self.pos += 1;
                    break;
                }
                if c == b'`' && d == b'\\' {
                    self.pos += 1;
                    self.read_nasm_escape(&mut buf, diags);
                } else {
                    let n = self.char_len();
                    buf.extend_from_slice(&self.bytes[self.pos..self.pos + n]);
                    self.pos += n;
                }
            }
            let idx = pool.add(buf);
            return Some(mk(TokKind::Str(idx), self));
        }
        let p = match self.bytes.get(self.pos..self.pos + 3) {
            Some(b">>>") => Some((Punct::Sar, 3)),
            Some(b"<<<") => Some((Punct::Shl, 3)),
            Some(b"<=>") => Some((Punct::Spaceship, 3)),
            _ => match self.bytes.get(self.pos..self.pos + 2) {
                Some(b"//") => Some((Punct::SlashSlash, 2)),
                Some(b"%%") => Some((Punct::PercentPercent, 2)),
                Some(b"^^") => Some((Punct::CaretCaret, 2)),
                _ => None,
            },
        };
        let (p, n) = p?;
        self.pos += n;
        Some(mk(TokKind::Punct(p), self))
    }

    /// A NASM number; see [`nasm_number`]. A run with a `.` in it, or an
    /// exponent after one, is a floating-point constant, which comes back as
    /// a [`TokKind::BadNumber`] carrying its text: only the data directives
    /// take one, and they read the text themselves.
    fn lex_nasm_number(&mut self, interner: &mut Interner) -> TokKind {
        let start = self.pos;
        let dollar = self.peek() == b'$';
        if dollar {
            self.pos += 1;
        }
        let mut float = false;
        while !self.at_end() {
            let b = self.peek();
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else if !dollar && b == b'.' && self.peek_at(1) != b'.' {
                float = true;
                self.pos += 1;
            } else if float
                && matches!(b, b'+' | b'-')
                && matches!(self.bytes[self.pos - 1], b'e' | b'E' | b'p' | b'P')
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        match nasm_number(text) {
            Some(v) if !float => TokKind::Int(v),
            _ => TokKind::BadNumber(interner.intern(text)),
        }
    }

    /// One escape in a NASM backquoted string, the backslash consumed. An
    /// unknown one stands for the character after the backslash, as in NASM.
    fn read_nasm_escape(&mut self, buf: &mut Vec<u8>, diags: &mut DiagBag) {
        let esc_start = self.pos - 1;
        if self.at_end() || self.peek() == b'\n' {
            return;
        }
        let c = self.peek();
        self.pos += 1;
        let digits = |this: &mut Self, radix: u32, max: usize| {
            let mut v = 0u32;
            for _ in 0..max {
                match (this.peek() as char).to_digit(radix) {
                    Some(d) => {
                        v = v.wrapping_mul(radix).wrapping_add(d);
                        this.pos += 1;
                    }
                    None => break,
                }
            }
            v
        };
        let byte = match c {
            b'a' => 7,
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'v' => 11,
            b'f' => 12,
            b'r' => 13,
            b'e' => 27,
            b'0'..=b'7' => {
                self.pos -= 1;
                digits(self, 8, 3) as u8
            }
            b'x' => digits(self, 16, 2) as u8,
            b'u' | b'U' => {
                let v = digits(self, 16, if c == b'u' { 4 } else { 8 });
                match char::from_u32(v) {
                    Some(ch) => {
                        let mut tmp = [0u8; 4];
                        buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                    }
                    None => diags.error(
                        self.span_from(esc_start),
                        format!(
                            "`{}` is not a Unicode character",
                            &self.src[esc_start..self.pos]
                        ),
                    ),
                }
                return;
            }
            other => other,
        };
        buf.push(byte);
    }

    fn lex_punct(&mut self, start: usize, spaced: bool, diags: &mut DiagBag) -> Token {
        use Punct::*;
        let two = |a: u8, b: u8| -> Option<Punct> {
            Some(match (a, b) {
                (b'<', b'<') => Shl,
                (b'>', b'>') => Shr,
                (b'=', b'=') => EqEq,
                (b'!', b'=') => Ne,
                (b'<', b'>') => Ne,
                (b'<', b'=') => Le,
                (b'>', b'=') => Ge,
                (b'&', b'&') => AndAnd,
                (b'|', b'|') => OrOr,
                _ => return None,
            })
        };
        let c = self.peek();
        if let Some(p) = two(c, self.peek_at(1)) {
            self.pos += 2;
            return Token {
                kind: TokKind::Punct(p),
                span: self.span_from(start),
                preceded_by_space: spaced,
            };
        }
        #[rustfmt::skip]
        let p = match c {
            b',' => Comma, b':' => Colon, b'(' => LParen, b')' => RParen,
            b'[' => LBracket, b']' => RBracket, b'{' => LBrace, b'}' => RBrace,
            b'+' => Plus, b'-' => Minus, b'*' => Star, b'/' => Slash,
            b'%' => Percent, b'&' => Amp, b'|' => Pipe, b'^' => Caret,
            b'~' => Tilde, b'!' => Bang, b'<' => Lt, b'>' => Gt, b'=' => Eq,
            b'@' => At, b'$' => Dollar, b'#' => Hash, b'.' => Dot,
            b'?' => Question, b'\\' => Backslash,
            _ => {
                // Consume one whole UTF-8 char so we do not split a codepoint.
                let ch = self.src[start..].chars().next().unwrap_or('\u{fffd}');
                self.pos += ch.len_utf8();
                let span = self.span_from(start);
                diags.emit(Diagnostic::error(span, format!("unexpected character `{ch}`")));
                return Token { kind: TokKind::Punct(Question), span, preceded_by_space: spaced };
            }
        };
        self.pos += 1;
        Token {
            kind: TokKind::Punct(p),
            span: self.span_from(start),
            preceded_by_space: spaced,
        }
    }

    /// A number written with a single-character radix prefix.
    ///
    /// Returns `None`, consuming nothing, unless the dialect has the prefix
    /// *and* a digit of that radix follows it. `%d0` and `$label` therefore
    /// fall through to be lexed as punctuation followed by a name.
    fn lex_prefixed_number(
        &mut self,
        start: usize,
        spaced: bool,
        diags: &mut DiagBag,
    ) -> Option<Token> {
        let c = self.peek() as char;
        let radix = self
            .config
            .number_prefixes
            .iter()
            .find(|(p, _)| *p == c)
            .map(|(_, r)| *r)?;
        if !(self.peek_at(1) as char).is_digit(radix) {
            return None;
        }
        self.pos += 1;
        let digits_start = self.pos;
        while !self.at_end() && (self.peek().is_ascii_alphanumeric() || self.peek() == b'_') {
            self.pos += 1;
        }
        let run = &self.src[digits_start..self.pos];
        let span = self.span_from(start);
        let mk = |kind| Token {
            kind,
            span,
            preceded_by_space: spaced,
        };
        let mut value: u64 = 0;
        for ch in run.chars().filter(|ch| *ch != '_') {
            let Some(d) = ch.to_digit(radix) else {
                diags.error(
                    span,
                    format!(
                        "invalid digit `{ch}` for base-{radix} literal `{}`",
                        &self.src[start..self.pos]
                    ),
                );
                return Some(mk(TokKind::Int(0)));
            };
            match value
                .checked_mul(radix as u64)
                .and_then(|v| v.checked_add(d as u64))
            {
                Some(v) => value = v,
                None => {
                    diags.error(span, "integer literal out of range for 64 bits");
                    return Some(mk(TokKind::Int(0)));
                }
            }
        }
        Some(mk(TokKind::Int(value)))
    }

    /// Whether the alphanumeric run at the cursor is a complete literal with a
    /// trailing radix letter, in the Renesas dialect.
    ///
    /// Renesas assemblers only have suffixes, so `0B00H` there can only be
    /// hex. NASM reads a `0b` prefix before it looks for a suffix and rejects
    /// the same text, so it keeps prefix-first order.
    fn suffixed_literal_ahead(&self) -> bool {
        let renesas = matches!(
            self.config.dialect,
            Dialect::Renesas | Dialect::CcRl | Dialect::CcRx | Dialect::EightBit
        );
        if !renesas && !self.config.radix_suffix {
            return false;
        }
        let mut p = self.pos;
        while p < self.bytes.len()
            && (self.bytes[p].is_ascii_alphanumeric() || self.bytes[p] == b'_')
        {
            p += 1;
        }
        let run = &self.src[self.pos..p];
        match suffix_radix(run) {
            // GNU as ports built with suffix numbers (RL78, MSP430) look for an
            // `h` before a `0b` prefix, so `0b1h` is 0xb1; a `0x` prefix still
            // wins, and makes `0x1fh` an error.
            Some(16) if !renesas => self.peek_at(1) | 0x20 == b'b',
            Some(_) => renesas,
            None => false,
        }
    }

    fn lex_number(
        &mut self,
        start: usize,
        spaced: bool,
        interner: &mut Interner,
        diags: &mut DiagBag,
    ) -> Token {
        let mk = |k: TokKind, this: &Self| Token {
            kind: k,
            span: this.span_from(start),
            preceded_by_space: spaced,
        };

        // GAS numeric local labels: `1f` / `1b` referring to a nearby `1:`.
        if self.config.local_label_refs && self.peek().is_ascii_digit() {
            let mut p = self.pos;
            while p < self.bytes.len() && self.bytes[p].is_ascii_digit() {
                p += 1;
            }
            let after = self.bytes.get(p).copied().unwrap_or(0);
            if (after == b'f' || after == b'b')
                && !is_ident_cont(self.bytes.get(p + 1).copied().unwrap_or(0))
            {
                let digits = &self.src[self.pos..p];
                // Only plain decimal runs are local labels; `0x1f` is a number.
                if (!digits.starts_with('0') || digits.len() == 1)
                    && let Ok(n) = digits.parse::<u32>()
                {
                    let dir = if after == b'f' {
                        LocalDir::Forward
                    } else {
                        LocalDir::Backward
                    };
                    self.pos = p + 1;
                    return mk(TokKind::LocalRef(n, dir), self);
                }
            }
        }

        // A `0x` / `0b` / `0o` prefix fixes the radix up front, unless the
        // whole run is a Renesas suffixed literal such as `0B00H`.
        let mut radix: u32 = 10;
        let mut digits_start = self.pos;
        if self.peek() == b'0' && !self.suffixed_literal_ahead() {
            let next = self.peek_at(1) | 0x20;
            let prefix_radix = match next {
                b'x' => Some(16),
                b'b' => Some(2),
                b'o' => Some(8),
                _ => None,
            };
            // Only treat it as a prefix if a valid digit actually follows,
            // so NASM's `0b` (binary zero) still lexes as a suffixed literal.
            if let Some(r) = prefix_radix
                && ((self.peek_at(2) as char).is_digit(r) || self.peek_at(2) == b'_')
            {
                radix = r;
                digits_start = self.pos + 2;
            }
        }
        let prefixed = digits_start != self.pos;
        self.pos = digits_start;

        // Scan the maximal alphanumeric run, then decide what it means. A
        // trailing radix letter can only be recognised once the run is known:
        // in `0ffh` the `f`s are digits and the `h` is the radix.
        let run_start = self.pos;
        while !self.at_end() && (self.peek().is_ascii_alphanumeric() || self.peek() == b'_') {
            self.pos += 1;
        }
        let mut run = &self.src[run_start..self.pos];

        if !prefixed {
            if self.config.radix_suffix
                && let Some(sr) = suffix_radix(run)
            {
                radix = sr;
                run = &run[..run.len() - 1];
            }
            if radix == 10
                && self.config.octal_leading_zero
                && run.len() > 1
                && run.starts_with('0')
            {
                radix = 8;
                run = &run[1..];
            }
        }

        if run.is_empty() || run.chars().all(|c| c == '_') {
            let span = self.span_from(start);
            diags.emit(Diagnostic::error(span, "integer literal has no digits"));
            return mk(TokKind::Int(0), self);
        }

        let mut value: u64 = 0;
        let mut overflow = false;
        for c in run.chars() {
            if c == '_' {
                continue;
            }
            let Some(d) = c.to_digit(radix) else {
                // Not reported here; see `TokKind::BadNumber`.
                let _ = c;
                let text = interner.intern(&self.src[start..self.pos]);
                return mk(TokKind::BadNumber(text), self);
            };
            match value
                .checked_mul(radix as u64)
                .and_then(|v| v.checked_add(d as u64))
            {
                Some(v) => value = v,
                None => overflow = true,
            }
        }

        // Not reported here either: a 128-bit checksum such as `.file`'s
        // `md5 0x...` is a number no expression can hold, but one directive
        // reads it from the token's text. Anywhere else the consumer reports
        // it, through `explain_bad_number`.
        if overflow {
            let text = interner.intern(&self.src[start..self.pos]);
            return mk(TokKind::BadNumber(text), self);
        }
        mk(TokKind::Int(value), self)
    }

    fn lex_string(
        &mut self,
        start: usize,
        spaced: bool,
        pool: &mut LitPool,
        diags: &mut DiagBag,
    ) -> Token {
        self.pos += 1; // opening quote
        let mut buf = Vec::new();
        loop {
            if self.at_end() || self.peek() == b'\n' {
                diags.emit(Diagnostic::error(
                    self.span_from(start),
                    "unterminated string literal",
                ));
                break;
            }
            let c = self.peek();
            if c == b'"' {
                self.pos += 1;
                if self.config.dialect.doubled_quotes() && self.peek() == b'"' {
                    buf.push(b'"');
                    self.pos += 1;
                    continue;
                }
                break;
            }
            if c == b'\\' && self.config.dialect.backslash_escapes() {
                self.pos += 1;
                self.read_escape(&mut buf, diags);
            } else {
                buf.push(c);
                self.pos += 1;
            }
        }
        let idx = pool.add(buf);
        Token {
            kind: TokKind::Str(idx),
            span: self.span_from(start),
            preceded_by_space: spaced,
        }
    }

    fn lex_char(&mut self, start: usize, spaced: bool, diags: &mut DiagBag) -> Token {
        self.pos += 1; // opening quote
        let mut buf = Vec::new();
        if self.config.char_multi {
            // NASM: everything up to the closing quote, packed big-endian.
            loop {
                if self.at_end() || self.peek() == b'\n' {
                    diags.emit(Diagnostic::error(
                        self.span_from(start),
                        "unterminated character literal",
                    ));
                    break;
                }
                if self.peek() == b'\'' {
                    self.pos += 1;
                    if self.config.dialect.doubled_quotes() && self.peek() == b'\'' {
                        buf.push(b'\'');
                        self.pos += 1;
                        continue;
                    }
                    break;
                }
                if self.peek() == b'\\' && self.config.dialect.backslash_escapes() {
                    self.pos += 1;
                    self.read_escape(&mut buf, diags);
                } else {
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
                    self.pos += ch.len_utf8();
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
            }
        } else {
            // GAS: exactly one character, closing quote optional (`.byte 'a`).
            if self.at_end() || self.peek() == b'\n' {
                let span = self.span_from(start);
                diags.emit(Diagnostic::error(span, "unterminated character literal"));
                return Token {
                    kind: TokKind::Int(0),
                    span,
                    preceded_by_space: spaced,
                };
            }
            if self.peek() == b'\\' {
                self.pos += 1;
                self.read_escape(&mut buf, diags);
            } else {
                let ch = self.src[self.pos..].chars().next().unwrap_or('\0');
                self.pos += ch.len_utf8();
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
            if self.peek() == b'\'' {
                self.pos += 1;
            }
        }
        // Multi-byte character constants pack big-endian, as GAS does.
        let mut v: u64 = 0;
        for &b in buf.iter().take(8) {
            v = (v << 8) | b as u64;
        }
        Token {
            kind: TokKind::Int(v),
            span: self.span_from(start),
            preceded_by_space: spaced,
        }
    }

    /// Reads one escape sequence, the leading backslash already consumed.
    fn read_escape(&mut self, buf: &mut Vec<u8>, diags: &mut DiagBag) {
        let esc_start = self.pos - 1;
        if self.at_end() {
            diags.emit(Diagnostic::error(
                self.span_from(esc_start),
                "trailing backslash",
            ));
            return;
        }
        let c = self.peek();
        self.pos += 1;
        let simple = match c {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'0'..=b'7' => None,
            b'a' => Some(0x07),
            b'b' => Some(0x08),
            b'e' => Some(0x1b),
            b'f' => Some(0x0c),
            b'v' => Some(0x0b),
            b'\\' => Some(b'\\'),
            b'\'' => Some(b'\''),
            b'"' => Some(b'"'),
            b'\n' => return, // escaped newline: nothing emitted
            b'x' => {
                let mut v: u32 = 0;
                let mut n = 0;
                while let Some(d) = (self.peek() as char).to_digit(16) {
                    v = v.wrapping_mul(16).wrapping_add(d);
                    self.pos += 1;
                    n += 1;
                }
                if n == 0 {
                    diags.emit(Diagnostic::error(
                        self.span_from(esc_start),
                        "`\\x` escape needs at least one hex digit",
                    ));
                }
                buf.push(v as u8);
                return;
            }
            other => {
                diags.emit(Diagnostic::warning(
                    self.span_from(esc_start),
                    format!("unknown escape `\\{}`", other as char),
                ));
                Some(other)
            }
        };
        match simple {
            Some(b) => buf.push(b),
            None => {
                // Octal: up to three digits, the first already consumed.
                let mut v: u32 = (c - b'0') as u32;
                for _ in 0..2 {
                    let d = self.peek();
                    if !(b'0'..=b'7').contains(&d) {
                        break;
                    }
                    v = v * 8 + (d - b'0') as u32;
                    self.pos += 1;
                }
                if v > 0xff {
                    diags.emit(Diagnostic::warning(
                        self.span_from(esc_start),
                        "octal escape out of range, truncated to 8 bits",
                    ));
                }
                buf.push(v as u8);
            }
        }
    }
}

/// The value of a NASM integer literal, or `None` if `text` is not one.
///
/// This is NASM 2.16.03's `readnum`: a radix prefix (`0x`, `0h`, `0d`,
/// `0t`, `0o`, `0q`, `0b`, `0y`, or `$`), a radix suffix (`h`, `x`, `d`,
/// `t`, `o`, `q`, `b`, `y`) or neither, with underscores anywhere. Where both
/// a prefix and a suffix could apply, the prefix wins if everything after it
/// is a digit of its radix: `0b101` is binary, `0b1h` hex.
/// Not API.
#[doc(hidden)]
pub fn nasm_number(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    let radix_letter = |c: u8| -> Option<u32> {
        Some(match c | 0x20 {
            b'b' | b'y' => 2,
            b'o' | b'q' => 8,
            b'd' | b't' => 10,
            b'h' | b'x' => 16,
            _ => return None,
        })
    };
    let digits = |s: &[u8], radix: u32| -> Option<u64> {
        let mut v: u64 = 0;
        let mut any = false;
        for &c in s {
            if c == b'_' {
                continue;
            }
            let d = (c as char).to_digit(radix)?;
            v = v.wrapping_mul(radix as u64).wrapping_add(d as u64);
            any = true;
        }
        any.then_some(v)
    };
    let len = b.len();
    let prefix = if len > 2 && b[0] == b'0' {
        radix_letter(b[1]).map(|r| (r, 2))
    } else if len > 1 && b[0] == b'$' {
        Some((16, 1))
    } else {
        None
    };
    if let Some((radix, skip)) = prefix
        && let Some(v) = digits(&b[skip..], radix)
    {
        return Some(v);
    }
    if len > 1
        && b[0] != b'$'
        && let Some(radix) = radix_letter(b[len - 1])
    {
        return digits(&b[..len - 1], radix);
    }
    if prefix.is_some() {
        return None;
    }
    digits(b, 10)
}

fn nasm_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || matches!(c, b'_' | b'.' | b'?') || c >= 0x80
}

fn nasm_name_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, b'_' | b'$' | b'#' | b'@' | b'~' | b'.' | b'?')
        || c >= 0x80
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'$' || c >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Harness {
        sm: SourceMap,
        interner: Interner,
        diags: DiagBag,
        pool: LitPool,
    }

    fn lex_all(src: &str, dialect: Dialect) -> (Vec<TokKind>, Harness) {
        let mut h = Harness {
            sm: SourceMap::new(),
            interner: Interner::new(),
            diags: DiagBag::new(),
            pool: LitPool::new(),
        };
        let f = h.sm.add("t.s", src);
        let mut kinds = Vec::new();
        {
            let mut lx = Lexer::new(&h.sm, f, LexConfig::for_dialect(dialect));
            loop {
                let t = lx.next_token(&mut h.interner, &mut h.pool, &mut h.diags);
                kinds.push(t.kind);
                if t.kind == TokKind::Eof {
                    break;
                }
            }
        }
        (kinds, h)
    }

    #[test]
    fn lexes_att_instruction() {
        let (k, h) = lex_all("movq %rax, %rbx\n", Dialect::Gas);
        assert_eq!(k[0], TokKind::Ident(h.interner.lookup("movq").unwrap()));
        assert_eq!(k[1], TokKind::Punct(Punct::Percent));
        assert_eq!(k[2], TokKind::Ident(h.interner.lookup("rax").unwrap()));
        assert_eq!(k[3], TokKind::Punct(Punct::Comma));
        assert_eq!(k[6], TokKind::Eol);
        assert_eq!(k[7], TokKind::Eof);
        assert!(!h.diags.has_errors());
    }

    #[test]
    fn number_bases() {
        let (k, _) = lex_all("0x1f 0b1010 0o17 42 0 1_000 010", Dialect::Gas);
        assert_eq!(
            &k[..7],
            &[
                TokKind::Int(31),
                TokKind::Int(10),
                TokKind::Int(15),
                TokKind::Int(42),
                TokKind::Int(0),
                TokKind::Int(1000),
                // A bare leading zero is octal in GAS.
                TokKind::Int(8),
            ]
        );
    }

    #[test]
    fn renesas_suffix_wins_over_a_look_alike_prefix() {
        let (k, _) = lex_all("0B00H 0b1h 0B0FH 0x1F 0b101", Dialect::Renesas);
        assert_eq!(
            &k[..5],
            &[
                TokKind::Int(0xb00),
                TokKind::Int(0xb1),
                TokKind::Int(0xb0f),
                TokKind::Int(0x1f),
                TokKind::Int(5),
            ]
        );
    }

    #[test]
    fn nasm_radix_suffixes() {
        let (k, _) = lex_all(
            "0ffh 1010b 17q 99d 0b1010 0xff 010 $0F 0h1f 1_000y 0b1h",
            Dialect::Nasm,
        );
        assert_eq!(
            &k[..11],
            &[
                TokKind::Int(255),
                TokKind::Int(10),
                TokKind::Int(15),
                TokKind::Int(99),
                TokKind::Int(10),
                TokKind::Int(255),
                // NASM has no leading-zero octal rule.
                TokKind::Int(10),
                TokKind::Int(15),
                TokKind::Int(31),
                TokKind::Int(8),
                // A prefix whose digits do not fit gives way to the suffix.
                TokKind::Int(0xb1),
            ]
        );
    }

    #[test]
    fn gas_local_label_refs_not_binary() {
        let (k, _) = lex_all("jmp 1b\njmp 2f\n", Dialect::Gas);
        assert_eq!(k[1], TokKind::LocalRef(1, LocalDir::Backward));
        assert_eq!(k[4], TokKind::LocalRef(2, LocalDir::Forward));
    }

    #[test]
    fn comment_styles_follow_dialect() {
        let (k, _) = lex_all("nop # gas comment\nnop", Dialect::Gas);
        assert_eq!(k[1], TokKind::Eol);
        // In GAS `;` separates statements rather than starting a comment.
        let (k, _) = lex_all("nop ; nop", Dialect::Gas);
        assert_eq!(k[1], TokKind::Eol);
        assert!(matches!(k[2], TokKind::Ident(_)));
        // In NASM it is a comment.
        let (k, _) = lex_all("nop ; nop", Dialect::Nasm);
        assert!(matches!(k[0], TokKind::Ident(_)));
        assert_eq!(k[1], TokKind::Eof);
    }

    #[test]
    fn line_start_comments_only_count_in_the_first_column() {
        // The ARM arrangement: `#` is an immediate prefix mid-line and a
        // comment only at the start of a line.
        let mut h = Harness {
            sm: SourceMap::new(),
            interner: Interner::new(),
            diags: DiagBag::new(),
            pool: LitPool::new(),
        };
        let f =
            h.sm.add("t.s", "# 1 \"file.c\"\n  # indented\nmov #1 @ gone\n");
        let mut cfg = LexConfig::for_dialect(Dialect::Gas);
        cfg.line_comment = vec!["@"];
        cfg.line_start_comment = vec!["#"];
        let mut kinds = Vec::new();
        {
            let mut lx = Lexer::new(&h.sm, f, cfg);
            loop {
                let t = lx.next_token(&mut h.interner, &mut h.pool, &mut h.diags);
                kinds.push(t.kind);
                if t.kind == TokKind::Eof {
                    break;
                }
            }
        }
        // Two comment-only lines, then `mov`, `#`, `1`, and the `@` comment.
        assert_eq!(kinds[0], TokKind::Eol);
        assert_eq!(kinds[1], TokKind::Eol);
        assert!(matches!(kinds[2], TokKind::Ident(_)));
        assert_eq!(kinds[3], TokKind::Punct(Punct::Hash));
        assert_eq!(kinds[4], TokKind::Int(1));
        assert_eq!(kinds[5], TokKind::Eol);
    }

    #[test]
    fn a_statement_separator_does_not_start_a_line() {
        // `a; # b` — GAS does not treat the `#` after `;` as first-column.
        let mut h = Harness {
            sm: SourceMap::new(),
            interner: Interner::new(),
            diags: DiagBag::new(),
            pool: LitPool::new(),
        };
        let f = h.sm.add("t.s", "nop; #1\n");
        let mut cfg = LexConfig::for_dialect(Dialect::Gas);
        cfg.line_comment = vec!["@"];
        cfg.line_start_comment = vec!["#"];
        let mut kinds = Vec::new();
        {
            let mut lx = Lexer::new(&h.sm, f, cfg);
            loop {
                let t = lx.next_token(&mut h.interner, &mut h.pool, &mut h.diags);
                kinds.push(t.kind);
                if t.kind == TokKind::Eof {
                    break;
                }
            }
        }
        assert_eq!(kinds[1], TokKind::Eol);
        assert_eq!(kinds[2], TokKind::Punct(Punct::Hash));
    }

    #[test]
    fn block_comments_and_continuations() {
        let (k, _) = lex_all("nop /* here\nand here */ nop \\\n nop\n", Dialect::Gas);
        assert!(matches!(k[0], TokKind::Ident(_)));
        assert!(matches!(k[1], TokKind::Ident(_)));
        assert!(matches!(k[2], TokKind::Ident(_)));
        assert_eq!(k[3], TokKind::Eol);
    }

    #[test]
    fn string_escapes() {
        let (k, h) = lex_all(r#" "a\nb\x41\101\\" "#, Dialect::Gas);
        let TokKind::Str(i) = k[0] else {
            panic!("not a string: {:?}", k[0])
        };
        assert_eq!(h.pool.get(i), b"a\nbAA\\");
        assert!(!h.diags.has_errors());
    }

    #[test]
    fn char_literals() {
        let (k, _) = lex_all("'a' '\\n'", Dialect::Gas);
        assert_eq!(k[0], TokKind::Int(b'a' as u64));
        assert_eq!(k[1], TokKind::Int(10));
        // GAS allows the closing quote to be omitted.
        let (k, _) = lex_all(".byte 'a, 'b", Dialect::Gas);
        assert_eq!(k[1], TokKind::Int(b'a' as u64));
        assert_eq!(k[2], TokKind::Punct(Punct::Comma));
        assert_eq!(k[3], TokKind::Int(b'b' as u64));
        // NASM quotes are strings, escapes only between backquotes; as a
        // number one is read little-endian, which is the consumer's job.
        let (k, h) = lex_all(r"'a\n' `a\tb`", Dialect::Nasm);
        let (TokKind::Str(a), TokKind::Str(b)) = (k[0], k[1]) else {
            panic!("not strings: {k:?}")
        };
        assert_eq!(h.pool.get(a), br"a\n");
        assert_eq!(h.pool.get(b), b"a\tb");
    }

    #[test]
    fn dot_is_location_counter_but_dot_ident_is_a_name() {
        let (k, h) = lex_all(".byte .", Dialect::Gas);
        assert_eq!(k[0], TokKind::Ident(h.interner.lookup(".byte").unwrap()));
        assert_eq!(k[1], TokKind::Punct(Punct::Dot));
    }

    #[test]
    fn a_malformed_number_is_classified_not_reported() {
        // `1to16` is an error in an expression but a broadcast count in an
        // AVX-512 decorator, and the lexer cannot tell which it is looking at.
        let (k, h) = lex_all("1to16 08", Dialect::Gas);
        let TokKind::BadNumber(n) = k[0] else {
            panic!("expected BadNumber, got {:?}", k[0])
        };
        assert_eq!(h.interner.get(n), "1to16");
        assert!(matches!(k[1], TokKind::BadNumber(_)));
        assert!(!h.diags.has_errors(), "the lexer must not judge it");
    }

    #[test]
    fn a_bad_number_explanation_names_the_digit_and_base() {
        assert_eq!(
            explain_bad_number("1st"),
            "invalid digit `s` for base-10 literal `1st`"
        );
        assert_eq!(
            explain_bad_number("08"),
            "invalid digit `8` for base-8 literal `08`"
        );
        assert_eq!(
            explain_bad_number("0xfg"),
            "invalid digit `g` for base-16 literal `0xfg`"
        );
    }

    #[test]
    fn reports_unterminated_string() {
        let (_, h) = lex_all("\"abc\n", Dialect::Gas);
        assert!(h.diags.has_errors());
    }

    #[test]
    fn multi_char_operators() {
        let (k, _) = lex_all("<< >> == != <= >= && ||", Dialect::Gas);
        use Punct::*;
        let want = [Shl, Shr, EqEq, Ne, Le, Ge, AndAnd, OrOr];
        for (i, p) in want.iter().enumerate() {
            assert_eq!(k[i], TokKind::Punct(*p), "at {i}");
        }
    }
}
