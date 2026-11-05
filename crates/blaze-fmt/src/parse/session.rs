//! Follows `src/tools/rustfmt/src/parse/session.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0), over blaze's source map: rustc's diagnostic context becomes a list of the parse
//! errors met, which rustfmt only counts, resets and prints.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::span::edition::Edition;
use crate::span::{BytePos, SourceMap, Span, symbol};

use crate::config::file_lines::LineRange;
use crate::ignore_path::IgnorePathSet;
use crate::parse::parser::{ModError, ModulePathSuccess};
use crate::source_map::LineRangeUtils;
use crate::utils::starts_with_newline;
use crate::visitor::SnippetProvider;
use crate::{Config, ErrorKind, FileName};

/// A parse error: rustfmt leaves the file alone and says why.
#[derive(Debug, Clone)]
pub(crate) struct ParseError {
    pub(crate) span: Span,
    pub(crate) message: String,
}

/// ParseSess holds structs necessary for constructing a parser.
pub(crate) struct ParseSess {
    source_map: Rc<SourceMap>,
    ignore_path_set: Arc<IgnorePathSet>,
    errors: RefCell<Vec<ParseError>>,
    can_reset_errors: Cell<bool>,
    silent: Cell<bool>,
    edition: Edition,
}

impl ParseSess {
    pub(crate) fn new(config: &Config) -> Result<ParseSess, ErrorKind> {
        let ignore_path_set = match IgnorePathSet::from_ignore_list(&config.ignore()) {
            Ok(ignore_path_set) => Arc::new(ignore_path_set),
            Err(e) => return Err(ErrorKind::InvalidGlobPattern(e)),
        };
        Ok(ParseSess {
            source_map: Rc::new(SourceMap::new()),
            ignore_path_set,
            errors: RefCell::new(Vec::new()),
            can_reset_errors: Cell::new(false),
            silent: Cell::new(!config.show_parse_errors()),
            edition: config.edition().into(),
        })
    }

    pub(crate) fn source_map(&self) -> &SourceMap {
        &self.source_map
    }

    pub(crate) fn edition(&self) -> Edition {
        self.edition
    }

    /// Determine the submodule path for the given module identifier.
    ///
    /// * `id` - The name of the module
    /// * `relative` - If Some(symbol), the symbol name is a directory relative to the dir_path.
    ///   If relative is Some, resolve the submodule at {dir_path}/{symbol}/{id}.rs
    ///   or {dir_path}/{symbol}/{id}/mod.rs. if None, resolve the module at {dir_path}/{id}.rs.
    /// *  `dir_path` - Module resolution will occur relative to this directory.
    pub(crate) fn default_submod_path(
        &self,
        id: symbol::Ident,
        relative: Option<symbol::Ident>,
        dir_path: &Path,
    ) -> Result<ModulePathSuccess, ModError> {
        crate::expand_module::default_submod_path(id, relative, dir_path).or_else(|e| {
            // If resolving a module relative to {dir_path}/{symbol} fails because a file
            // could not be found, then try to resolve the module relative to {dir_path}.
            // If we still can't find the module after searching for it in {dir_path},
            // surface the original error.
            if matches!(e, ModError::FileNotFound(..)) && relative.is_some() {
                crate::expand_module::default_submod_path(id, None, dir_path).map_err(|_| e)
            } else {
                Err(e)
            }
        })
    }

    pub(crate) fn is_file_parsed(&self, path: &Path) -> bool {
        self.source_map
            .get_source_file(&crate::span::FileName::Real(
                crate::span::RealFileName::from_path(path),
            ))
            .is_some()
    }

    pub(crate) fn ignore_file(&self, path: &FileName) -> bool {
        self.ignore_path_set.as_ref().is_match(path)
    }

    pub(crate) fn set_silent_emitter(&mut self) {
        self.silent.set(true);
    }

    pub(crate) fn span_to_filename(&self, span: Span) -> FileName {
        self.source_map.span_to_filename(span).into()
    }

    pub(crate) fn span_to_file_contents(&self, span: Span) -> Arc<crate::span::SourceFile> {
        self.source_map.lookup_source_file(span.data().lo)
    }

    pub(crate) fn span_to_first_line_string(&self, span: Span) -> String {
        let file_lines = self.source_map.span_to_lines(span).ok();

        match file_lines {
            Some(fl) => fl
                .file
                .get_line(fl.lines[0].line_index)
                .map_or_else(String::new, |s| s.to_string()),
            None => String::new(),
        }
    }

    pub(crate) fn line_of_byte_pos(&self, pos: BytePos) -> usize {
        self.source_map.lookup_char_pos(pos).line
    }

    // TODO(calebcartwright): Preemptive, currently unused addition
    // that will be used to support formatting scenarios that take original
    // positions into account
    /// Determines whether two byte positions are in the same source line.
    #[allow(dead_code)]
    pub(crate) fn byte_pos_same_line(&self, a: BytePos, b: BytePos) -> bool {
        self.line_of_byte_pos(a) == self.line_of_byte_pos(b)
    }

    pub(crate) fn span_to_debug_info(&self, span: Span) -> String {
        self.source_map.span_to_diagnostic_string(span)
    }

    pub(crate) fn snippet_provider(&self, span: Span) -> SnippetProvider {
        let source_file = self.source_map.lookup_char_pos(span.lo()).file;
        SnippetProvider::new(
            source_file.start_pos,
            source_file.end_position(),
            Arc::clone(source_file.src.as_ref().unwrap()),
        )
    }

    pub(crate) fn get_original_snippet(&self, filename: &FileName) -> Option<Arc<String>> {
        let rustc_filename = match filename {
            FileName::Real(path) => {
                crate::span::FileName::Real(crate::span::RealFileName::from_path(path))
            }
            FileName::Stdin => crate::span::FileName::Custom("stdin".to_owned()),
        };

        self.source_map
            .get_source_file(&rustc_filename)
            .and_then(|source_file| source_file.src.clone())
    }
}

// Methods that should be restricted within the parse module.
impl ParseSess {
    /// Records a parse error and, unless parse errors are hidden or the file is ignored,
    /// prints it.
    pub(crate) fn emit_error(&self, span: Span, message: impl Into<String>) {
        let error = ParseError {
            span,
            message: message.into(),
        };
        let file = self.source_map.span_to_filename(span);
        let ignored = match &file {
            crate::span::FileName::Real(real) => real.local_path().is_some_and(|p| {
                self.ignore_path_set
                    .is_match(&FileName::Real(p.to_path_buf()))
            }),
            crate::span::FileName::Custom(_) => false,
        };
        if ignored {
            if self.errors.borrow().is_empty() {
                self.can_reset_errors.set(true);
            }
        } else {
            self.can_reset_errors.set(false);
            if !self.silent.get() {
                print_error(&self.source_map, &error);
            }
        }
        self.errors.borrow_mut().push(error);
    }

    pub(super) fn can_reset_errors(&self) -> bool {
        self.can_reset_errors.get()
    }

    pub(crate) fn has_errors(&self) -> bool {
        !self.errors.borrow().is_empty()
    }

    pub(crate) fn reset_errors(&self) {
        self.errors.borrow_mut().clear();
    }
}

/// `error: message` and where, as a compiler would point at it.
#[allow(clippy::print_stderr)]
fn print_error(source_map: &SourceMap, error: &ParseError) {
    let loc = source_map.lookup_char_pos(error.span.lo());
    eprintln!(
        "error: {}\n --> {}:{}:{}",
        error.message,
        loc.file.name,
        loc.line,
        loc.col.0 + 1
    );
}

impl LineRangeUtils for ParseSess {
    fn lookup_line_range(&self, span: Span) -> LineRange {
        let snippet = self.source_map.span_to_snippet(span).unwrap_or_default();
        let lo = self.source_map.lookup_line(span.lo()).unwrap();
        let hi = self.source_map.lookup_line(span.hi()).unwrap();

        debug_assert_eq!(
            lo.sf.name, hi.sf.name,
            "span crossed file boundary: lo: {lo:?}, hi: {hi:?}"
        );

        // in case the span starts with a newline, the line range is off by 1 without the
        // adjustment below
        let offset = 1 + if starts_with_newline(&snippet) { 1 } else { 0 };
        // Line numbers start at 1
        LineRange {
            file: lo.sf.clone(),
            lo: lo.line + offset,
            hi: hi.line + offset,
        }
    }
}
