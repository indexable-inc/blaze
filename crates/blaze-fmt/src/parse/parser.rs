//! Follows `src/tools/rustfmt/src/parse/parser.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0). A file is lexed by rustc's rules (`parse::lexer`), parsed by rust-analyzer and
//! lowered to rustc's AST with rustc's spans (`parse::lower`); a lexer error, a parse error, or
//! a construct the lowering does not cover yet is a parse error, and rustfmt leaves the file
//! alone.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ast::ThinVec;
use crate::ast::{self, attr};
use crate::parse::lexer::{self, StripTokens};
use crate::parse::lower::{DelimIndex, Lower};
use crate::parse::stream::parse_ra;
use crate::span::{BytePos, SourceFile, Span, sym};
use syntax::view::{self as ra, AstNode};

use crate::Input;
use crate::parse::session::ParseSess;

pub(crate) type DirectoryOwnership = crate::expand_module::DirOwnership;
pub(crate) type ModulePathSuccess = crate::expand_module::ModulePathSuccess;
pub(crate) type ModError = crate::expand_module::ModError;

#[derive(Clone)]
pub(crate) struct Directory {
    pub(crate) path: PathBuf,
    pub(crate) ownership: DirectoryOwnership,
}

/// A parser for Rust source code.
pub(crate) struct Parser<'a> {
    psess: &'a ParseSess,
    file: Arc<SourceFile>,
}

/// A builder for the `Parser`.
#[derive(Default)]
pub(crate) struct ParserBuilder<'a> {
    psess: Option<&'a ParseSess>,
    input: Option<Input>,
}

impl<'a> ParserBuilder<'a> {
    pub(crate) fn input(mut self, input: Input) -> ParserBuilder<'a> {
        self.input = Some(input);
        self
    }

    pub(crate) fn psess(mut self, psess: &'a ParseSess) -> ParserBuilder<'a> {
        self.psess = Some(psess);
        self
    }

    pub(crate) fn build(self) -> Result<Parser<'a>, ParserError> {
        let psess = self.psess.ok_or(ParserError::NoParseSess)?;
        let input = self.input.ok_or(ParserError::NoInput)?;

        let file = match input {
            Input::File(ref path) => load_file(psess, path, None)?,
            Input::Text(text) => psess
                .source_map()
                .new_source_file(crate::span::FileName::Custom("stdin".to_owned()), text),
        };
        Ok(Parser { psess, file })
    }
}

/// rustc's `new_parser_from_file`: a file that cannot be read is an error at `sp`, the `mod`
/// item that named it.
fn load_file(
    psess: &ParseSess,
    path: &Path,
    sp: Option<Span>,
) -> Result<Arc<SourceFile>, ParserError> {
    match psess.source_map().load_file(path) {
        Ok(file) => Ok(file),
        Err(e) => {
            let span = sp.unwrap_or_default();
            psess.emit_error(span, format!("couldn't read `{}`: {e}", path.display()));
            Err(ParserError::ParserCreationError)
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum ParserError {
    NoParseSess,
    NoInput,
    ParserCreationError,
    ParseError,
    ParsePanicError,
}

/// A file parsed and lowered: rustc's `parse_mod(Eof)`.
fn parse_mod(psess: &ParseSess, file: &SourceFile) -> Option<ast::Crate> {
    let src = file.src.as_ref()?;
    let base = file.start_pos;
    let edition = psess.edition();
    let stream =
        match lexer::lex_token_trees(src, base, edition, StripTokens::ShebangAndFrontmatter) {
            Ok(stream) => stream,
            Err(err) => {
                psess.emit_error(err.span, err.message);
                return None;
            }
        };
    let delims = DelimIndex::new(&stream);
    let parse = match parse_ra(src, edition.to_ra(), parser::TopEntryPoint::SourceFile) {
        Ok(parse) => parse,
        Err((offset, message)) => {
            let at = base + BytePos(offset as u32);
            psess.emit_error(Span::with_root_ctxt(at, at), message);
            return None;
        }
    };
    let tree = ra::SourceFile::cast(parse.root()).expect("a source file parses to one");
    let lower = Lower::new(base, edition, &delims);
    let krate = lower.krate(tree);
    match lower.finish() {
        Ok(()) => Some(krate),
        Err(unsupported) if unsupported.rustc_rejects => {
            psess.emit_error(unsupported.span, unsupported.message);
            None
        }
        Err(unsupported) => {
            psess.emit_error(
                unsupported.span,
                format!(
                    "blaze fmt cannot format this file yet: {}",
                    unsupported.message
                ),
            );
            None
        }
    }
}

impl<'a> Parser<'a> {
    pub(crate) fn submod_path_from_attr(attrs: &[ast::Attribute], path: &Path) -> Option<PathBuf> {
        let path_sym = attr::first_attr_value_str_by_name(attrs, sym::path)?;
        let path_str = path_sym.as_str();

        // On windows, the base path might have the form
        // `\\?\foo\bar` in which case it does not tolerate
        // mixed `/` and `\` separators, so canonicalize
        // `/` to `\`.
        #[cfg(windows)]
        let path_str = path_str.replace("/", "\\");

        Some(path.join(path_str))
    }

    pub(crate) fn parse_file_as_module(
        psess: &'a ParseSess,
        path: &Path,
        span: Span,
    ) -> Result<(ast::AttrVec, ThinVec<Box<ast::Item>>, Span), ParserError> {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let file = load_file(psess, path, Some(span)).ok()?;
            match parse_mod(psess, &file) {
                Some(krate) => Some((krate.attrs, krate.items, krate.spans.inner_span)),
                None => {
                    if psess.can_reset_errors() {
                        psess.reset_errors();
                    }
                    None
                }
            }
        }));
        match result {
            Ok(Some(m)) if !psess.has_errors() => Ok(m),
            Ok(Some(m)) if psess.can_reset_errors() => {
                psess.reset_errors();
                Ok(m)
            }
            Ok(_) => Err(ParserError::ParseError),
            Err(..) if path.exists() => Err(ParserError::ParseError),
            Err(_) => Err(ParserError::ParsePanicError),
        }
    }

    pub(crate) fn parse_crate(
        input: Input,
        psess: &'a ParseSess,
    ) -> Result<ast::Crate, ParserError> {
        let krate = Parser::parse_crate_inner(input, psess)?;
        if !psess.has_errors() {
            return Ok(krate);
        }

        if psess.can_reset_errors() {
            psess.reset_errors();
            return Ok(krate);
        }

        Err(ParserError::ParseError)
    }

    fn parse_crate_inner(input: Input, psess: &'a ParseSess) -> Result<ast::Crate, ParserError> {
        ParserBuilder::default()
            .input(input)
            .psess(psess)
            .build()?
            .parse_crate_mod()
    }

    fn parse_crate_mod(&mut self) -> Result<ast::Crate, ParserError> {
        let (psess, file) = (self.psess, &self.file);
        match catch_unwind(AssertUnwindSafe(|| parse_mod(psess, file))) {
            Ok(Some(k)) => Ok(k),
            Ok(None) => Err(ParserError::ParseError),
            Err(_) => Err(ParserError::ParsePanicError),
        }
    }
}
