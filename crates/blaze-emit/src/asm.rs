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
//! Numeric label definitions name the next instruction address. The mnemonic table encodes
//! instructions. An unknown mnemonic, a directive (`.arch_extension`), a named label or an
//! unsupported operand form yields [`AsmError`] with the template line and token.
//!
//! [`Assembler::feed`] appends instruction words directly to the caller's code buffer.
//! [`Assembler::finish`] resolves local branches within this template. Only labels and
//! their uses need scratch space. Each statement fits in [`STATEMENT_MOST`] bytes.

use alloc::vec::Vec;
mod table;

use core::{alloc::Allocator, ops::Range};

use blaze_own::{AVec, Alloc};

use crate::enc::Field;

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

/// An occurrence of a local number, retaining its exact spelling for diagnostics.
struct Local {
    token: Range<usize>,
    at: usize,
    line: u32,
}

impl Local {
    fn new(spellings: &mut AVec<'_, u8>, token: &[u8], at: usize, line: u32) -> Self {
        let start = spellings.len();
        spellings.extend_from_slice(token);
        Self { token: start..spellings.len(), at, line }
    }

    /// Compare decimal numbers without an integer-size limit. Zero has an empty key.
    fn number<'a>(&self, spellings: &'a [u8]) -> &'a [u8] {
        let digits = &spellings[self.token.start..self.token.end - 1];
        &digits[digits.iter().position(|&d| d != b'0').unwrap_or(digits.len())..]
    }

    fn error(&self, spellings: &[u8], why: &'static str) -> AsmError {
        AsmError { line: self.line, why, token: AsmToken::of(&spellings[self.token.clone()]) }
    }
}

/// The template reader of one `asm!` block.
pub(crate) struct Assembler<'out, 's, A: Allocator> {
    code: &'out mut Vec<u8, A>,
    spellings: AVec<'s, u8>,
    definitions: AVec<'s, Local>,
    branches: AVec<'s, (Local, Field, u32)>,
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
}

impl<'out, 's, A: Allocator> Assembler<'out, 's, A> {
    pub(crate) fn new(dialect: Dialect, code: &'out mut Vec<u8, A>, scratch: Alloc<'s>) -> Self {
        Assembler {
            code,
            spellings: AVec::new_in(scratch),
            definitions: AVec::new_in(scratch),
            branches: AVec::new_in(scratch),
            dialect,
            lex: Lex::Code,
            slash: false,
            statement: [0; STATEMENT_MOST],
            len: 0,
            statement_line: 1,
            line: 1,
        }
    }

    /// Reads another rendered piece of the template.
    pub(crate) fn feed(&mut self, text: &[u8]) -> Result<(), AsmError> {
        for &byte in text {
            self.byte(byte)?;
        }
        Ok(())
    }

    /// Sort D definitions once, then find each branch's nearest definition in O(log D).
    pub(crate) fn finish(mut self) -> Result<(), AsmError> {
        if self.slash {
            self.slash = false;
            self.push(b'/')?;
        }
        if let Lex::BlockComment { .. } = self.lex {
            return Err(AsmError { line: self.line, why: "an unterminated `/*` comment", token: AsmToken::of(b"/*") });
        }
        self.end_statement()?;
        let names = &self.spellings;
        self.definitions.sort_unstable_by(|a, b| {
            (a.number(names), a.at, a.token.start).cmp(&(b.number(names), b.at, b.token.start))
        });
        for pair in self.definitions.windows(2) {
            if pair[0].at == pair[1].at && pair[0].number(names) == pair[1].number(names) {
                return Err(pair[1].error(names, "a local label defined twice at the same instruction address"));
            }
        }
        for (branch, field, word) in self.branches {
            let number = branch.number(names);
            // A definition immediately before a branch is at its PC and is backward.
            let next = self.definitions.partition_point(|d| (d.number(names), d.at) <= (number, branch.at));
            let target = if names[branch.token.end - 1] == b'f' {
                self.definitions.get(next)
            } else {
                next.checked_sub(1).and_then(|i| self.definitions.get(i))
            };
            let target = target.filter(|d| d.number(names) == number)
                .ok_or_else(|| branch.error(names, "no local label in the requested direction"))?;
            let bytes = target.at as i64 - branch.at as i64;
            let patched = field.patch(word, bytes)
                .ok_or_else(|| branch.error(names, "local label outside this branch's displacement range"))?;
            self.code[branch.at..branch.at + 4].copy_from_slice(&patched.to_le_bytes());
        }
        Ok(())
    }

    fn byte(&mut self, byte: u8) -> Result<(), AsmError> {
        match self.lex {
            Lex::LineComment => {
                if byte == b'\n' {
                    self.lex = Lex::Code;
                    self.newline()?;
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
                    b'\n' => self.newline(),
                    b':' => {
                        self.push(byte)?;
                        let token = trim(&self.statement[..self.len]);
                        let digits = &token[..token.len() - 1];
                        if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
                            self.definitions.push(Local::new(&mut self.spellings, token, self.code.len(), self.statement_line));
                            self.len = 0;
                        }
                        Ok(())
                    }
                    b';' => match self.dialect {
                        Dialect::Darwin => {
                            self.lex = Lex::LineComment;
                            Ok(())
                        }
                        Dialect::Elf => self.end_statement(),
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

    fn newline(&mut self) -> Result<(), AsmError> {
        self.end_statement()?;
        self.line += 1;
        Ok(())
    }

    /// Assembles the statement held, if it is not blank, and empties the buffer.
    fn end_statement(&mut self) -> Result<(), AsmError> {
        let len = core::mem::replace(&mut self.len, 0);
        let line = self.statement_line;
        self.statement_line = self.line;
        let text = trim(&self.statement[..len]);
        if text.is_empty() {
            return Ok(());
        }
        let instruction = table::assemble(text).map_err(|(why, token)| AsmError { line, why, token: AsmToken::of(token) })?;
        let word = match instruction {
            table::Instruction::Word(word) => word,
            table::Instruction::Branch { word, field, target } => {
                let local = Local::new(&mut self.spellings, target, self.code.len(), line);
                self.branches.push((local, field, word));
                word
            }
        };
        self.code.extend_from_slice(&word.to_le_bytes());
        Ok(())
    }
}

/// `text` without its leading and trailing ASCII blanks.
pub(crate) fn trim(text: &[u8]) -> &[u8] {
    let start = text.iter().position(|it| !it.is_ascii_whitespace()).unwrap_or(text.len());
    let end = text.iter().rposition(|it| !it.is_ascii_whitespace()).map_or(start, |it| it + 1);
    &text[start..end]
}
