//! The macro uses of one file, read from its tokens: every `#[derive(..)]` name, every other
//! attribute (unwrapping `cfg_attr` and `unsafe(..)`), every function-like call `path!(..)`,
//! every `macro_rules!` definition, and what a procedural-macro crate exports
//! (`#[proc_macro]`, `#[proc_macro_attribute]`, `#[proc_macro_derive(Name, attributes(..))]`).
//!
//! It is a census, not name resolution: a use is named by its path as written, so a caller that
//! wants to know which macro a name is classifies it by the names the workspace and its
//! dependencies define ([`Event::RulesDef`], [`Event::Export`]) and by the built-in lists here.
//! A `macro_rules!` body is skipped whole: the calls in a template are not calls.
//!
//! Every loop moves forward over the file's significant tokens, so a scan is bounded by them;
//! a nested `cfg_attr` is read to [`NEST_BOUND`] levels.

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};

/// How deep `cfg_attr(.., cfg_attr(..))` and `unsafe(..)` are unwrapped.
pub const NEST_BOUND: u32 = 8;

/// What a procedural-macro crate exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExportKind {
    /// `#[proc_macro]`: a function-like macro.
    FnLike,
    /// `#[proc_macro_attribute]`.
    Attr,
    /// `#[proc_macro_derive(..)]`.
    Derive,
}

/// One use or definition, its text borrowed from the file; `at` is its byte offset in the file
/// (printed as a line by the caller, rule 71).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'t> {
    /// A name in `#[derive(..)]`.
    Derive { path: &'t str, name: &'t str, at: u32 },
    /// Any other attribute: `#[path ..]` or `#![path ..]`.
    Attr { path: &'t str, name: &'t str, at: u32, inner: bool },
    /// `path!(..)`, `path![..]` or `path! { .. }`.
    Call { path: &'t str, name: &'t str, at: u32 },
    /// `macro_rules! name`.
    RulesDef { name: &'t str, at: u32 },
    /// A procedural macro this file exports.
    Export { kind: ExportKind, name: &'t str, at: u32 },
    /// A helper attribute of the derive `derive` this file exports.
    Helper { derive: &'t str, name: &'t str },
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: TokenKind,
    start: u32,
    end: u32,
}

/// Reads `text`'s macro uses, calling `each` for each in the order written. A file longer than
/// `u32::MAX` bytes is read to that length.
pub fn scan<'t>(text: &'t str, cpu: Cpu, alloc: Alloc<'_>, each: &mut impl FnMut(Event<'t>)) {
    let mut toks: AVec<'_, Tok> = AVec::new_in(alloc);
    let mut at = 0u32;
    for token in tokenize(text, FrontmatterAllowed::No, cpu) {
        let start = at;
        at = at.saturating_add(token.len);
        match token.kind {
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => {}
            kind => toks.push(Tok { kind, start, end: at }),
        }
    }
    let file = Scan { text, toks: &toks };
    let mut i = 0;
    while i < toks.len() {
        i = file.step(i, each);
    }
}

struct Scan<'t, 'v> {
    text: &'t str,
    toks: &'v [Tok],
}

impl<'t> Scan<'t, '_> {
    fn kind(&self, i: usize) -> Option<TokenKind> {
        self.toks.get(i).map(|tok| tok.kind)
    }

    fn word(&self, i: usize) -> &'t str {
        match self.toks.get(i) {
            Some(tok) => self.text.get(tok.start as usize..tok.end as usize).unwrap_or(""),
            None => "",
        }
    }

    fn is_word(&self, i: usize) -> bool {
        matches!(self.kind(i), Some(TokenKind::Ident | TokenKind::RawIdent))
    }

    fn opens(&self, i: usize) -> bool {
        matches!(self.kind(i), Some(TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace))
    }

    /// The index of the token that closes the one opened at `open`, or the end.
    fn close(&self, open: usize) -> usize {
        let mut depth = 0u32;
        let mut i = open;
        while i < self.toks.len() {
            match self.toks[i].kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        self.toks.len()
    }

    /// `::` at `i` and `i + 1`, written as one.
    fn path_sep(&self, i: usize) -> bool {
        match (self.toks.get(i), self.toks.get(i + 1)) {
            (Some(a), Some(b)) => a.kind == TokenKind::Colon && b.kind == TokenKind::Colon && a.end == b.start,
            _ => false,
        }
    }

    /// The end (exclusive) of the path starting at `i` (`a::b::c`, a leading `::` allowed).
    fn path_end(&self, mut i: usize, stop: usize) -> usize {
        if self.path_sep(i) {
            i += 2;
        }
        while i < stop && self.is_word(i) {
            if i + 3 <= stop && self.path_sep(i + 1) && self.is_word(i + 3) {
                i += 3;
            } else {
                return i + 1;
            }
        }
        i
    }

    fn slice(&self, from: usize, to: usize) -> &'t str {
        match (self.toks.get(from), to.checked_sub(1).and_then(|last| self.toks.get(last))) {
            (Some(a), Some(b)) if b.end >= a.start => self.text.get(a.start as usize..b.end as usize).unwrap_or(""),
            _ => "",
        }
    }

    /// Reads from token `i`; returns where to go on.
    fn step(&self, i: usize, each: &mut impl FnMut(Event<'t>)) -> usize {
        let Some(kind) = self.kind(i) else { return i + 1 };
        match kind {
            TokenKind::Pound => {
                let inner = self.kind(i + 1) == Some(TokenKind::Bang);
                let open = if inner { i + 2 } else { i + 1 };
                if self.kind(open) != Some(TokenKind::OpenBracket) {
                    return i + 1;
                }
                let close = self.close(open);
                let export = self.attr(open + 1, close, inner, 0, each);
                if let Some(kind) = export {
                    self.export_fn(close + 1, kind, each);
                }
                close + 1
            }
            TokenKind::Ident if self.word(i) == "macro_rules" && self.kind(i + 1) == Some(TokenKind::Bang) && self.is_word(i + 2) => {
                each(Event::RulesDef { name: raw(self.word(i + 2)), at: self.toks[i + 2].start });
                if self.opens(i + 3) { self.close(i + 3) + 1 } else { i + 3 }
            }
            TokenKind::Ident | TokenKind::RawIdent
                if self.kind(i + 1) == Some(TokenKind::Bang) && self.opens(i + 2) && !KEYWORDS.contains(&self.word(i)) =>
            {
                // Back over `seg::` pairs (and a `$crate` or a leading `::`) to the path's start.
                let mut from = i;
                while from >= 3 && self.path_sep(from - 2) && self.is_word(from - 3) {
                    from -= 3;
                }
                if from >= 1 && self.word(from) == "crate" && self.kind(from - 1) == Some(TokenKind::Dollar) {
                    from -= 1;
                } else if from >= 2 && self.path_sep(from - 2) {
                    from -= 2;
                }
                each(Event::Call { path: self.slice(from, i + 1), name: raw(self.word(i)), at: self.toks[from].start });
                i + 2
            }
            _ => i + 1,
        }
    }

    /// One attribute's tokens, `from..to`: its uses, and the export it makes, if any.
    fn attr(&self, from: usize, to: usize, inner: bool, depth: u32, each: &mut impl FnMut(Event<'t>)) -> Option<ExportKind> {
        if depth > NEST_BOUND || from >= to || !(self.is_word(from) || self.path_sep(from)) {
            return None;
        }
        let end = self.path_end(from, to);
        let path = self.slice(from, end);
        let name = raw(self.word(end.saturating_sub(1)));
        let args = (end < to && self.kind(end) == Some(TokenKind::OpenParen)).then(|| (end + 1, self.close(end).min(to)));
        match (path, args) {
            ("derive", Some((a, b))) => {
                for (p, q) in self.parts(a, b) {
                    if p < q {
                        let e = self.path_end(p, q);
                        each(Event::Derive { path: self.slice(p, e), name: raw(self.word(e.saturating_sub(1))), at: self.toks[p].start });
                    }
                }
                None
            }
            ("cfg_attr", Some((a, b))) => {
                for (p, q) in self.parts(a, b).skip(1) {
                    if let Some(kind) = self.attr(p, q, inner, depth + 1, each) {
                        return Some(kind);
                    }
                }
                None
            }
            ("unsafe", Some((a, b))) => self.attr(a, b, inner, depth + 1, each),
            ("proc_macro", _) => Some(ExportKind::FnLike),
            ("proc_macro_attribute", _) => Some(ExportKind::Attr),
            ("proc_macro_derive", Some((a, b))) => {
                let mut parts = self.parts(a, b);
                let Some((p, _)) = parts.next() else { return None };
                let derive = raw(self.word(p));
                each(Event::Export { kind: ExportKind::Derive, name: derive, at: self.toks[p].start });
                for (p, q) in parts {
                    if self.word(p) == "attributes" && self.kind(p + 1) == Some(TokenKind::OpenParen) {
                        let close = self.close(p + 1).min(q);
                        for (h, _) in self.parts(p + 2, close) {
                            if self.is_word(h) {
                                each(Event::Helper { derive, name: raw(self.word(h)) });
                            }
                        }
                    }
                }
                None
            }
            _ => {
                each(Event::Attr { path, name, at: self.toks[from].start, inner });
                None
            }
        }
    }

    /// After a `#[proc_macro]` or `#[proc_macro_attribute]`: the next `fn`'s name, past other
    /// attributes and qualifiers.
    fn export_fn(&self, mut i: usize, kind: ExportKind, each: &mut impl FnMut(Event<'t>)) {
        // A few qualifiers and attributes at most stand between the attribute and the `fn`.
        let stop = (i + 64).min(self.toks.len());
        while i < stop {
            if self.kind(i) == Some(TokenKind::Pound) && self.kind(i + 1) == Some(TokenKind::OpenBracket) {
                i = self.close(i + 1) + 1;
                continue;
            }
            if self.word(i) == "fn" && self.is_word(i + 1) {
                each(Event::Export { kind, name: raw(self.word(i + 1)), at: self.toks[i + 1].start });
                return;
            }
            i += 1;
        }
    }

    /// The comma-separated parts of `from..to` at depth 0, each as `(start, end)`.
    fn parts(&self, from: usize, to: usize) -> Parts<'_, 't> {
        Parts { scan: self, at: from, to }
    }
}

struct Parts<'s, 't> {
    scan: &'s Scan<'t, 's>,
    at: usize,
    to: usize,
}

impl Iterator for Parts<'_, '_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        if self.at >= self.to {
            return None;
        }
        let start = self.at;
        let mut i = start;
        while i < self.to {
            if self.scan.opens(i) {
                i = self.scan.close(i) + 1;
                continue;
            }
            if self.scan.kind(i) == Some(TokenKind::Comma) {
                break;
            }
            i += 1;
        }
        let end = i.min(self.to);
        self.at = end + 1;
        Some((start, end))
    }
}

fn raw(word: &str) -> &str {
    word.strip_prefix("r#").unwrap_or(word)
}

/// Words that may stand before `!` without being a macro's name (`if !(..)`, `return !x`).
const KEYWORDS: [&str; 38] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
    "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
];

/// The derives the language and core define.
pub const BUILTIN_DERIVES: [&str; 10] =
    ["Clone", "Copy", "Debug", "Default", "Eq", "PartialEq", "Ord", "PartialOrd", "Hash", "CoercePointee"];

/// Attributes rustc reads itself (its built-in attribute table, the parts real code writes).
pub const BUILTIN_ATTRS: [&str; 75] = [
    "allow", "warn", "deny", "forbid", "expect", "cfg", "cfg_attr", "test", "ignore", "should_panic", "bench",
    "derive", "automatically_derived", "macro_export", "macro_use", "proc_macro", "proc_macro_derive",
    "proc_macro_attribute", "must_use", "deprecated", "doc", "inline", "cold", "no_mangle", "export_name",
    "link_section", "link", "link_name", "link_ordinal", "repr", "path", "recursion_limit", "type_length_limit",
    "no_std", "no_implicit_prelude", "no_main", "crate_name", "crate_type", "feature", "non_exhaustive",
    "track_caller", "target_feature", "instruction_set", "used", "global_allocator", "windows_subsystem",
    "panic_handler", "debugger_visualizer", "collapse_debuginfo", "coverage", "optimize", "register_tool",
    "naked", "test_runner", "default", "may_dangle", "unstable", "stable", "rustc_const_unstable",
    "rustc_const_stable", "macro_escape", "no_link", "no_builtins", "compiler_builtins", "lang", "start",
    "thread_local", "ffi_const", "ffi_pure", "patchable_function_entry", "sanitize", "unsafe", "allow_internal_unstable",
    "do_not_recommend", "on_unimplemented",
];

/// Tools whose attributes rustc accepts by their first segment.
pub const TOOLS: [&str; 6] = ["rustfmt", "clippy", "rustdoc", "diagnostic", "rust_analyzer", "miri"];

/// The macros core, alloc and std define, by name.
pub const STD_MACROS: [&str; 53] = [
    "println", "print", "eprintln", "eprint", "format", "format_args", "write", "writeln", "vec", "panic", "assert",
    "assert_eq", "assert_ne", "debug_assert", "debug_assert_eq", "debug_assert_ne", "unreachable", "unimplemented",
    "todo", "matches", "concat", "stringify", "include", "include_str", "include_bytes", "env", "option_env", "cfg",
    "line", "column", "file", "module_path", "compile_error", "dbg", "thread_local", "r#try", "try",
    "is_x86_feature_detected", "is_aarch64_feature_detected", "addr_of", "addr_of_mut", "offset_of", "pin",
    "assert_matches", "debug_assert_matches", "const_format_args", "format_args_nl", "asm", "global_asm",
    "naked_asm", "concat_bytes", "trace_macros", "cfg_match",
];

/// An attribute path rustc reads itself.
pub fn builtin_attr(path: &str) -> bool {
    match path.split_once("::") {
        Some((first, _)) => TOOLS.contains(&first.trim()),
        None => BUILTIN_ATTRS.contains(&path) || path.starts_with("rustc_"),
    }
}

/// A derive path core defines (`Debug`, `core::fmt::Debug`, `std::hash::Hash`).
pub fn builtin_derive(path: &str, name: &str) -> bool {
    BUILTIN_DERIVES.contains(&name) && (path == name || std_path(path))
}

/// A call path of a macro core, alloc or std defines.
pub fn std_macro(path: &str, name: &str) -> bool {
    STD_MACROS.contains(&name) && (path == name || std_path(path))
}

fn std_path(path: &str) -> bool {
    let path = path.trim_start_matches("::");
    ["std::", "core::", "alloc::"].iter().any(|root| path.starts_with(root))
}
