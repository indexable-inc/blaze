//! Symbols and identifiers, following `compiler/rustc_span/src/symbol.rs` at rust-lang/rust
//! 12433c307 (MIT or Apache-2.0): its keywords, and which of them are reserved in which edition.
//!
//! A symbol is its interned text; two symbols are equal when their text is.

use std::fmt;

use super::edition::{Edition, session_edition};
use super::{DUMMY_SP, Span};

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(&'static str);

impl Symbol {
    pub fn intern(s: &str) -> Symbol {
        Symbol(intern::Symbol::intern(s).as_str())
    }

    pub const fn new_static(s: &'static str) -> Symbol {
        Symbol(s)
    }

    pub fn as_str(&self) -> &str {
        self.0
    }

    fn is_special(self) -> bool {
        matches!(self.0, "$crate" | "{{root}}" | "_")
    }

    fn is_used_keyword_always(self) -> bool {
        matches!(
            self.0,
            "as" | "break"
                | "const"
                | "continue"
                | "crate"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        )
    }

    fn is_unused_keyword_always(self) -> bool {
        matches!(
            self.0,
            "abstract"
                | "become"
                | "box"
                | "do"
                | "final"
                | "macro"
                | "override"
                | "priv"
                | "typeof"
                | "unsized"
                | "virtual"
                | "yield"
        )
    }

    fn is_used_keyword_conditional(self, edition: Edition) -> bool {
        matches!(self.0, "async" | "await" | "dyn") && edition >= Edition::Edition2018
    }

    fn is_unused_keyword_conditional(self, edition: Edition) -> bool {
        self.0 == "gen" && edition.at_least_rust_2024()
            || self.0 == "try" && edition.at_least_rust_2018()
    }

    pub fn is_reserved(self, edition: impl Copy + FnOnce() -> Edition) -> bool {
        self.is_special()
            || self.is_used_keyword_always()
            || self.is_unused_keyword_always()
            || self.is_used_keyword_conditional(edition())
            || self.is_unused_keyword_conditional(edition())
    }

    pub fn is_path_segment_keyword(self) -> bool {
        matches!(
            self.0,
            "super" | "self" | "Self" | "crate" | "{{root}}" | "$crate"
        )
    }

    pub fn is_bool_lit(self) -> bool {
        self == kw::True || self == kw::False
    }

    pub fn can_be_raw(self) -> bool {
        !self.0.is_empty() && self != kw::Underscore && !self.is_path_segment_keyword()
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.0, f)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.0, f)
    }
}

/// An identifier with where it was written. As rustc's, two are equal when their names are
/// (and their hygiene contexts, which are all the root here): the span's position is not part of
/// it, so `use a::b as b` is a rename to the same name.
#[derive(Clone, Copy)]
pub struct Ident {
    pub name: Symbol,
    pub span: Span,
}

impl PartialEq for Ident {
    fn eq(&self, rhs: &Ident) -> bool {
        self.name == rhs.name
    }
}

impl Eq for Ident {}

impl std::hash::Hash for Ident {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Ident {
    pub fn new(name: Symbol, span: Span) -> Ident {
        Ident { name, span }
    }

    pub fn with_dummy_span(name: Symbol) -> Ident {
        Ident::new(name, DUMMY_SP)
    }

    pub fn from_str(s: &str) -> Ident {
        Ident::with_dummy_span(Symbol::intern(s))
    }

    pub fn as_str(&self) -> &str {
        self.name.as_str()
    }

    pub fn is_special(self) -> bool {
        self.name.is_special()
    }

    pub fn is_used_keyword(self) -> bool {
        self.name.is_used_keyword_always()
            || self.name.is_used_keyword_conditional(session_edition())
    }

    pub fn is_unused_keyword(self) -> bool {
        self.name.is_unused_keyword_always()
            || self.name.is_unused_keyword_conditional(session_edition())
    }

    pub fn is_reserved(self) -> bool {
        self.name.is_reserved(session_edition)
    }

    pub fn is_path_segment_keyword(self) -> bool {
        self.name.is_path_segment_keyword()
    }

    pub fn is_raw_guess(self) -> bool {
        self.name.can_be_raw() && self.is_reserved()
    }

    pub fn without_first_quote(self) -> Ident {
        Ident::new(
            Symbol::intern(self.as_str().trim_start_matches('\'')),
            self.span,
        )
    }
}

impl fmt::Debug for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)?;
        fmt::Debug::fmt(&self.span, f)
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_raw_guess() {
            f.write_str("r#")?;
        }
        fmt::Display::fmt(&self.name, f)
    }
}

/// The keywords and special identifiers, as `rustc_span::symbol::kw` names them.
#[allow(non_upper_case_globals)]
pub mod kw {
    use super::Symbol;

    macro_rules! keywords {
        ($($name:ident: $text:literal,)*) => {
            $(pub const $name: Symbol = Symbol::new_static($text);)*
        };
    }

    keywords! {
        DollarCrate: "$crate",
        PathRoot: "{{root}}",
        Underscore: "_",
        As: "as",
        Break: "break",
        Const: "const",
        Continue: "continue",
        Crate: "crate",
        Else: "else",
        Enum: "enum",
        Extern: "extern",
        False: "false",
        Fn: "fn",
        For: "for",
        If: "if",
        Impl: "impl",
        In: "in",
        Let: "let",
        Loop: "loop",
        Match: "match",
        Mod: "mod",
        Move: "move",
        Mut: "mut",
        Pub: "pub",
        Ref: "ref",
        Return: "return",
        SelfLower: "self",
        SelfUpper: "Self",
        Static: "static",
        Struct: "struct",
        Super: "super",
        Trait: "trait",
        True: "true",
        Type: "type",
        Unsafe: "unsafe",
        Use: "use",
        Where: "where",
        While: "while",
        Abstract: "abstract",
        Become: "become",
        Box: "box",
        Do: "do",
        Final: "final",
        Macro: "macro",
        Override: "override",
        Priv: "priv",
        Typeof: "typeof",
        Unsized: "unsized",
        Virtual: "virtual",
        Yield: "yield",
        Async: "async",
        Await: "await",
        Dyn: "dyn",
        Gen: "gen",
        Try: "try",
        StaticLifetime: "'static",
        UnderscoreLifetime: "'_",
        Auto: "auto",
        Builtin: "builtin",
        Catch: "catch",
        Default: "default",
        MacroRules: "macro_rules",
        Pin: "pin",
        Raw: "raw",
        Reuse: "reuse",
        Safe: "safe",
        Union: "union",
        Yeet: "yeet",
    }
}

/// The pre-interned symbols rustfmt names.
#[allow(non_upper_case_globals)]
pub mod sym {
    use super::Symbol;

    macro_rules! symbols {
        ($($name:ident,)*) => {
            $(pub const $name: Symbol = Symbol::new_static(stringify!($name));)*
        };
    }

    symbols! {
        C,
        allow,
        attribute,
        automatically_derived,
        block,
        bool,
        char,
        deny,
        deprecated,
        drop,
        expect,
        forbid,
        hidden,
        keyword,
        note,
        proc_macro,
        proc_macro_attribute,
        proc_macro_derive,
        rustc_doc_primitive,
        str,
        warn,
        cfg,
        cfg_attr,
        derive,
        doc,
        expr,
        expr_2021,
        f128,
        f16,
        f32,
        f64,
        guard,
        i128,
        i16,
        i32,
        i64,
        i8,
        ident,
        isize,
        item,
        lifetime,
        literal,
        macro_use,
        meta,
        pat,
        pat_param,
        path,
        rustfmt,
        skip,
        stmt,
        tt,
        ty,
        u128,
        u16,
        u32,
        u64,
        u8,
        underscore_imports,
        usize,
        vis,
    }

    pub const empty: Symbol = Symbol::new_static("");
}
