//! The `asm!` template reader of the baseline tier (pass/asm.rs feeds it): the template's text,
//! its placeholders already rendered as register names, read the way LLVM's AArch64 assembler
//! reads inline assembly, each statement assembled by the mnemonic table (asm/table.rs).
//!
//! What LLVM's assembler takes as a comment (llvm/lib/MC/MCParser/AsmLexer.cpp `LexSlash`,
//! `LexLineComment` and the `#` at a statement's start; AArch64MCAsmInfo.cpp, the comment and
//! separator strings), checked against the sites tempo's dependencies hold:
//! - `/* .. */` anywhere, also across lines (compact_str `"/* {value} */"`, zmij `"/*{0}*/"`,
//!   constant_time_eq `"/* {0} */"`); an unterminated one is refused, as LLVM refuses it.
//! - `//` to the end of the line, on every AArch64 target.
//! - `#` as the first character of a statement that is not blank (zeroize `"# {}"`); a `#`
//!   later in a statement is an immediate's mark (`msr dit, #1`).
//! - `;`: on Apple targets the comment string (AArch64MCAsmInfoDarwin), so the rest of the line
//!   is a comment; on ELF targets the statement separator (AArch64MCAsmInfoELF).
//!
//! Statements end at a newline (the Reference: several template strings are joined with `\n`)
//! or, on ELF, at `;`. A statement left blank once its comments are gone assembles to nothing:
//! a comment-only template emits no instruction, which is what an optimization barrier is.
//! Every other statement is a mnemonic and its operands, encoded by its row of the table; a
//! mnemonic with no row, a directive (`.arch_extension`), a label (`2:`) or an operand form no
//! row takes is [`AsmError`], naming the template's line and the token (rule 19). Nothing is
//! skipped or guessed (rule 3).
//!
//! Sans-IO: [`Assembler::feed`] takes bytes and hands each instruction word to a sink;
//! [`Assembler::finish`] ends the last statement. No allocation: one statement is held in a
//! fixed buffer of [`STATEMENT_MOST`] bytes, and a longer one is refused by name.

mod table;

/// The most bytes one statement holds once its comments are gone.
pub(crate) const STATEMENT_MOST: usize = 256;

/// How `;` is read: the target's object format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dialect {
    /// Apple (Mach-O): `;` starts a comment.
    Darwin,
    /// ELF (Linux): `;` separates statements.
    Elf,
}

impl Dialect {
    pub(crate) const fn of(os: crate::Os) -> Dialect {
        match os {
            crate::Os::MacOs => Dialect::Darwin,
            crate::Os::Linux => Dialect::Elf,
        }
    }
}

/// The most bytes of a token an error keeps.
const TOKEN_MOST: usize = 24;

/// The token of the template an `asm!` refusal names: its first [`TOKEN_MOST`] bytes, and
/// whether there were more.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AsmToken {
    bytes: [u8; TOKEN_MOST],
    len: u8,
    cut: bool,
}

impl AsmToken {
    pub(crate) fn of(text: &[u8]) -> AsmToken {
        let mut bytes = [0u8; TOKEN_MOST];
        let len = text.len().min(TOKEN_MOST);
        bytes[..len].copy_from_slice(&text[..len]);
        AsmToken { bytes, len: len as u8, cut: text.len() > TOKEN_MOST }
    }

    /// The kept bytes as text (a token cut inside a character shows up to its last whole one).
    pub fn text(&self) -> &str {
        let kept = &self.bytes[..self.len as usize];
        match core::str::from_utf8(kept) {
            Ok(it) => it,
            // `valid_up_to` is a character boundary of `kept`.
            Err(e) => core::str::from_utf8(&kept[..e.valid_up_to()]).unwrap_or(""),
        }
    }

    /// Whether the token was longer than what is kept.
    pub fn cut(&self) -> bool {
        self.cut
    }
}

impl core::fmt::Display for AsmToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.cut {
            true => write!(f, "`{}..`", self.text()),
            false => write!(f, "`{}`", self.text()),
        }
    }
}

/// Why a template was not assembled: the line of the template (from 1) and the token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AsmError {
    pub(crate) line: u32,
    pub(crate) why: &'static str,
    pub(crate) token: AsmToken,
}

/// Where the reader is in the text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lex {
    Code,
    /// After `//`, `;` (Apple) or a statement's first `#`: up to the newline.
    LineComment,
    /// Inside `/* .. */`; `star` when the last byte was `*`.
    BlockComment { star: bool },
}

/// The template reader of one `asm!` block.
pub(crate) struct Assembler {
    dialect: Dialect,
    lex: Lex,
    /// A `/` seen in code, not yet known to start a comment.
    slash: bool,
    /// The statement so far, its comments gone.
    statement: [u8; STATEMENT_MOST],
    len: usize,
    /// The line the statement is on (from 1) and the line the reader is on.
    statement_line: u32,
    line: u32,
    /// The instruction words handed out.
    words: u32,
}

impl Assembler {
    pub(crate) fn new(dialect: Dialect) -> Assembler {
        Assembler {
            dialect,
            lex: Lex::Code,
            slash: false,
            statement: [0; STATEMENT_MOST],
            len: 0,
            statement_line: 1,
            line: 1,
            words: 0,
        }
    }

    /// Reads `text`, handing each instruction word it assembles to `out`.
    pub(crate) fn feed(&mut self, text: &[u8], out: &mut dyn FnMut(u32)) -> Result<(), AsmError> {
        for &byte in text {
            self.byte(byte, out)?;
        }
        Ok(())
    }

    /// Ends the template: its last statement assembled. Answers how many words it made.
    pub(crate) fn finish(&mut self, out: &mut dyn FnMut(u32)) -> Result<u32, AsmError> {
        if self.slash {
            self.slash = false;
            self.push(b'/')?;
        }
        if let Lex::BlockComment { .. } = self.lex {
            return Err(AsmError { line: self.line, why: "an unterminated `/*` comment", token: AsmToken::of(b"/*") });
        }
        self.end_statement(out)?;
        Ok(self.words)
    }

    fn byte(&mut self, byte: u8, out: &mut dyn FnMut(u32)) -> Result<(), AsmError> {
        match self.lex {
            Lex::LineComment => {
                if byte == b'\n' {
                    self.lex = Lex::Code;
                    self.newline(out)?;
                }
                Ok(())
            }
            Lex::BlockComment { star } => {
                self.lex = match (star, byte) {
                    (true, b'/') => Lex::Code,
                    (_, b'*') => Lex::BlockComment { star: true },
                    _ => Lex::BlockComment { star: false },
                };
                // A comment spanning lines leaves the statement it began in open, as LLVM's
                // lexer does (the comment is one token of trivia); the line count still moves.
                if byte == b'\n' {
                    self.line += 1;
                }
                Ok(())
            }
            Lex::Code => {
                if self.slash {
                    self.slash = false;
                    match byte {
                        b'/' => {
                            self.lex = Lex::LineComment;
                            return Ok(());
                        }
                        b'*' => {
                            self.lex = Lex::BlockComment { star: false };
                            return Ok(());
                        }
                        _ => self.push(b'/')?,
                    }
                }
                match byte {
                    b'/' => {
                        self.slash = true;
                        Ok(())
                    }
                    b'\n' => self.newline(out),
                    b';' => match self.dialect {
                        Dialect::Darwin => {
                            self.lex = Lex::LineComment;
                            Ok(())
                        }
                        Dialect::Elf => self.end_statement(out),
                    },
                    b'#' if self.blank() => {
                        self.lex = Lex::LineComment;
                        Ok(())
                    }
                    _ => self.push(byte),
                }
            }
        }
    }

    /// Whether the statement so far is only blanks.
    fn blank(&self) -> bool {
        self.statement[..self.len].iter().all(|it| it.is_ascii_whitespace())
    }

    fn push(&mut self, byte: u8) -> Result<(), AsmError> {
        if self.len == STATEMENT_MOST {
            return Err(AsmError {
                line: self.statement_line,
                why: "a statement longer than the assembler's 256-byte statement buffer",
                token: AsmToken::of(&self.statement[..self.len]),
            });
        }
        if self.len == 0 || self.blank() {
            self.statement_line = self.line;
        }
        self.statement[self.len] = byte;
        self.len += 1;
        Ok(())
    }

    fn newline(&mut self, out: &mut dyn FnMut(u32)) -> Result<(), AsmError> {
        self.end_statement(out)?;
        self.line += 1;
        Ok(())
    }

    /// Assembles the statement held, if it is not blank, and empties the buffer.
    fn end_statement(&mut self, out: &mut dyn FnMut(u32)) -> Result<(), AsmError> {
        let len = core::mem::replace(&mut self.len, 0);
        let line = self.statement_line;
        self.statement_line = self.line;
        let text = trim(&self.statement[..len]);
        if text.is_empty() {
            return Ok(());
        }
        let word = table::assemble(text).map_err(|(why, token)| AsmError { line, why, token: AsmToken::of(token) })?;
        out(word);
        self.words += 1;
        Ok(())
    }
}

/// `text` without its leading and trailing ASCII blanks.
pub(crate) fn trim(text: &[u8]) -> &[u8] {
    let start = text.iter().position(|it| !it.is_ascii_whitespace()).unwrap_or(text.len());
    let end = text.iter().rposition(|it| !it.is_ascii_whitespace()).map_or(start, |it| it + 1);
    &text[start..end]
}
