//! GNU linker scripts given as shared libraries, as glibc ships `libc.so`:
//! `GROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a
//! AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )`, and Debian's `libgcc_s.so`:
//! `GROUP ( libgcc_s.so.1 -lgcc )`. Only the commands that name inputs are read.

use crate::model::{Names, Shared};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token<'a> {
    Word(&'a str),
    Open,
    Close,
    Comma,
    Semicolon,
}

pub(super) fn read(name: &str, bytes: &[u8]) -> Result<Shared, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        // alloc: error path
        format!("{name}: neither an ELF file, an archive nor a linker script (not UTF-8 text)")
    })?;
    let mut lexer = Lexer { name, rest: text };
    // alloc: once per file, `Shared::inputs`
    let mut inputs = Vec::new();
    while let Some(token) = lexer.next()? {
        let command = match token {
            Token::Semicolon => continue,
            Token::Word(command) => command,
            other => {
                // alloc: error path
                return Err(format!("{name}: linker script: expected a command, found {other:?}"));
            }
        };
        if lexer.next()? != Some(Token::Open) {
            // alloc: error path
            return Err(format!("{name}: linker script: {command} without its arguments"));
        }
        match command {
            "GROUP" | "INPUT" => list(&mut lexer, false, &mut inputs)?,
            "OUTPUT_FORMAT" | "OUTPUT_ARCH" => {
                let mut depth = 1;
                while depth > 0 {
                    match lexer.next()? {
                        Some(Token::Open) => depth += 1,
                        Some(Token::Close) => depth -= 1,
                        Some(_) => {}
                        None => {
                            // alloc: error path
                            return Err(format!("{name}: linker script: {command}( is not closed"));
                        }
                    }
                }
            }
            _ => {
                // alloc: error path
                return Err(format!(
                    "{name}: linker script command {command} is not supported: blaze reads only \
                     GROUP, INPUT, AS_NEEDED, OUTPUT_FORMAT and OUTPUT_ARCH"
                ));
            }
        }
    }
    if inputs.is_empty() {
        return Err(format!("{name}: a linker script that names no inputs")); // alloc: error path
    }
    Ok(Shared {
        name: super::file_name(name),
        symbols: Vec::new(), // alloc: empty, allocates nothing
        names: Names::default(),
        versions: Vec::new(), // alloc: empty, allocates nothing
        more: Vec::new(), // alloc: empty, allocates nothing
        inputs,
    })
}

/// Reads the file names of a `GROUP`, `INPUT` or `AS_NEEDED`, from just past its `(` through
/// its `)`, into `inputs`.
fn list(lexer: &mut Lexer<'_, '_>, as_needed: bool, inputs: &mut Vec<String>) -> Result<(), String> {
    let name = lexer.name;
    loop {
        let Some(token) = lexer.next()? else {
            return Err(format!("{name}: linker script: a file list is not closed")); // alloc: error path
        };
        match token {
            Token::Close => return Ok(()),
            Token::Comma => {}
            Token::Word("AS_NEEDED") if lexer.peek()? == Some(Token::Open) => {
                if as_needed {
                    // alloc: error path
                    return Err(format!("{name}: linker script: AS_NEEDED inside AS_NEEDED"));
                }
                lexer.next()?;
                list(lexer, true, inputs)?;
            }
            Token::Word("-l" | "-l:") => {
                // alloc: error path
                return Err(format!("{name}: linker script: -l names no library"));
            }
            // `-lNAME` and `-l:FILE` too, as written: `target::locate` searches for them and the
            // engine finds them on the line.
            Token::Word(file) => inputs.push(file.to_owned()), // alloc: once per input the script names, `Shared::inputs`
            other => {
                // alloc: error path
                return Err(format!(
                    "{name}: linker script: expected a file name, found {other:?}"
                ));
            }
        }
    }
}

/// The script's tokens, read one at a time from what is left of its text; a copy peeks.
#[derive(Clone, Copy)]
struct Lexer<'n, 'a> {
    name: &'n str,
    rest: &'a str,
}

impl<'a> Lexer<'_, 'a> {
    fn peek(&self) -> Result<Option<Token<'a>>, String> {
        let mut ahead = *self;
        ahead.next()
    }

    fn next(&mut self) -> Result<Option<Token<'a>>, String> {
        let name = self.name;
        loop {
            let rest = self.rest.trim_start();
            let Some(first) = rest.chars().next() else {
                self.rest = rest;
                return Ok(None);
            };
            if let Some(comment) = rest.strip_prefix("/*") {
                let end = comment
                    .find("*/")
                    // alloc: error path
                    .ok_or_else(|| format!("{name}: linker script: a comment is not closed"))?;
                self.rest = &comment[end + 2..];
                continue;
            }
            let token = match first {
                '(' => Token::Open,
                ')' => Token::Close,
                ',' => Token::Comma,
                ';' => Token::Semicolon,
                '"' => {
                    let quoted = &rest[1..];
                    let end = quoted
                        .find('"')
                        // alloc: error path
                        .ok_or_else(|| format!("{name}: linker script: a quoted name is not closed"))?;
                    self.rest = &quoted[end + 1..];
                    return Ok(Some(Token::Word(&quoted[..end])));
                }
                _ => {
                    let end = rest
                        .find(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '"'))
                        .unwrap_or(rest.len());
                    let end = rest[..end].find("/*").unwrap_or(end);
                    self.rest = &rest[end..];
                    return Ok(Some(Token::Word(&rest[..end])));
                }
            };
            self.rest = &rest[1..];
            return Ok(Some(token));
        }
    }
}
