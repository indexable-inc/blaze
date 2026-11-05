//! Intra-doc links: which links in a doc comment name an item, and what they resolve to.
//!
//! Which Markdown links are intra-doc links, and the disambiguators, are rustdoc's
//! (`preprocess_link`, `Disambiguator::from_str`, `should_ignore_link` and `resolve_primitive` in
//! `passes/collect_intra_doc_links.rs`, `strip_generics_from_path` in
//! `compiler/rustc_resolve/src/rustdoc.rs`, rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0);
//! resolving the path is rust-analyzer's, in the scope of the item the docs are on.

use std::ops::Range;

use base_db::Db;
use hir::{DocLinkDef, ModuleDef, Namespace};
use pulldown_cmark::{BrokenLink, CowStr, Event, LinkType, Options, Parser, Tag};

/// The Markdown options rustdoc renders doc bodies with (`main_body_opts`).
pub(crate) fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
}

/// A link in Markdown as pulldown-cmark finds it.
#[derive(Debug, Clone)]
pub(crate) struct MarkdownLink {
    pub(crate) kind: LinkType,
    /// The destination; for a shortcut link with no definition (`[Foo]`), its text.
    pub(crate) link: String,
    pub(crate) range: Range<usize>,
}

/// Every link in `markdown`, those with no destination included as their text.
pub(crate) fn markdown_links(markdown: &str) -> Vec<MarkdownLink> {
    fn keep_text<'a>(broken: BrokenLink<'a>) -> Option<(CowStr<'a>, CowStr<'a>)> {
        Some((broken.reference, "".into()))
    }
    let mut callback = keep_text;
    let parser = Parser::new_with_broken_link_callback(markdown, options(), Some(&mut callback));
    let mut out = vec![];
    for (event, range) in parser.into_offset_iter() {
        if let Event::Start(Tag::Link { link_type: kind, dest_url: dest, .. }) = event {
            out.push(MarkdownLink { kind, link: dest.to_string(), range });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disambiguator {
    Primitive,
    Struct,
    Enum,
    Trait,
    Union,
    Module,
    Const,
    Static,
    Function,
    Derive,
    Field,
    Variant,
    TypeAlias,
    Macro,
    TypeNs,
    ValueNs,
    MacroNs,
}

impl Disambiguator {
    fn ns(self) -> Namespace {
        match self {
            Disambiguator::Primitive
            | Disambiguator::Struct
            | Disambiguator::Enum
            | Disambiguator::Trait
            | Disambiguator::Union
            | Disambiguator::Module
            | Disambiguator::Variant
            | Disambiguator::TypeAlias
            | Disambiguator::TypeNs => Namespace::Types,
            Disambiguator::Const
            | Disambiguator::Static
            | Disambiguator::Function
            | Disambiguator::Field
            | Disambiguator::ValueNs => Namespace::Values,
            Disambiguator::Derive | Disambiguator::Macro | Disambiguator::MacroNs => {
                Namespace::Macros
            }
        }
    }

    /// Whether `def` is the kind the disambiguator asks for.
    fn accepts(self, def: &ModuleDef) -> bool {
        match self {
            Disambiguator::Struct => matches!(def, ModuleDef::Adt(hir::Adt::Struct(_))),
            Disambiguator::Enum => matches!(def, ModuleDef::Adt(hir::Adt::Enum(_))),
            Disambiguator::Union => matches!(def, ModuleDef::Adt(hir::Adt::Union(_))),
            Disambiguator::Trait => matches!(def, ModuleDef::Trait(_)),
            Disambiguator::Module => matches!(def, ModuleDef::Module(_)),
            Disambiguator::Const => matches!(def, ModuleDef::Const(_)),
            Disambiguator::Static => matches!(def, ModuleDef::Static(_)),
            Disambiguator::Function => matches!(def, ModuleDef::Function(_)),
            Disambiguator::Variant => matches!(def, ModuleDef::EnumVariant(_)),
            Disambiguator::TypeAlias => matches!(def, ModuleDef::TypeAlias(_)),
            Disambiguator::Primitive => matches!(def, ModuleDef::BuiltinType(_)),
            Disambiguator::Derive | Disambiguator::Macro => matches!(def, ModuleDef::Macro(_)),
            Disambiguator::Field
            | Disambiguator::TypeNs
            | Disambiguator::ValueNs
            | Disambiguator::MacroNs => true,
        }
    }

    /// `(disambiguator, path, link text)` of a link, `None` when it has none, `Err` when it has
    /// one rustdoc does not know.
    fn from_str(link: &str) -> Result<Option<(Self, &str, &str)>, String> {
        let suffixes = [
            ("!()", Disambiguator::Macro),
            ("!{}", Disambiguator::Macro),
            ("![]", Disambiguator::Macro),
            ("()", Disambiguator::Function),
            ("!", Disambiguator::Macro),
        ];
        if let Some(idx) = link.find('@') {
            let (prefix, rest) = link.split_at(idx);
            let d = match prefix {
                "struct" => Disambiguator::Struct,
                "enum" => Disambiguator::Enum,
                "trait" => Disambiguator::Trait,
                "union" => Disambiguator::Union,
                "module" | "mod" => Disambiguator::Module,
                "const" | "constant" => Disambiguator::Const,
                "static" => Disambiguator::Static,
                "function" | "fn" | "method" => Disambiguator::Function,
                "derive" => Disambiguator::Derive,
                "field" => Disambiguator::Field,
                "variant" => Disambiguator::Variant,
                "type" => Disambiguator::TypeNs,
                "value" => Disambiguator::ValueNs,
                "macro" => Disambiguator::MacroNs,
                "prim" | "primitive" => Disambiguator::Primitive,
                "tyalias" | "typealias" => Disambiguator::TypeAlias,
                _ => return Err(format!("unknown disambiguator `{prefix}`")),
            };
            for (suffix, kind) in suffixes {
                if let Some(path) = rest.strip_suffix(suffix) {
                    if d.ns() != kind.ns() {
                        return Err(format!(
                            "unmatched disambiguator `{prefix}` and suffix `{suffix}`"
                        ));
                    } else if path.len() > 1 {
                        return Ok(Some((d, &path[1..], &rest[1..])));
                    }
                }
            }
            Ok(Some((d, &rest[1..], &rest[1..])))
        } else {
            for (suffix, kind) in suffixes {
                if let Some(path) = link.strip_suffix(suffix)
                    && !path.is_empty()
                {
                    return Ok(Some((kind, path, link)));
                }
            }
            Ok(None)
        }
    }
}

/// An intra-doc link ready to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Preprocessed {
    pub(crate) path: String,
    pub(crate) disambiguator: Option<Disambiguator>,
    pub(crate) fragment: Option<String>,
    /// What the link shows when its text is the link itself: the link without a prefix
    /// disambiguator.
    pub(crate) text: String,
}

/// Whether a Markdown link is an intra-doc link, and its path if so. `Some(Err)` is a link
/// rustdoc warns about without resolving.
pub(crate) fn preprocess(link: &MarkdownLink) -> Option<Result<Preprocessed, String>> {
    let can_be_url = !matches!(
        link.kind,
        LinkType::ShortcutUnknown | LinkType::CollapsedUnknown | LinkType::ReferenceUnknown
    );
    if link.link.is_empty() {
        return None;
    }
    if can_be_url && link.link.contains('/') {
        return None;
    }
    let stripped = link.link.replace('`', "");
    let mut parts = stripped.split('#');
    let path = parts.next().unwrap().trim();
    if path.is_empty() {
        return None;
    }
    let fragment = parts.next();
    if parts.next().is_some() {
        return Some(Err("has an issue with the link anchor: multiple anchors".to_owned()));
    }
    let (disambiguator, path, text) = match Disambiguator::from_str(path) {
        Ok(Some((d, path, text))) => (Some(d), path.trim(), text.trim()),
        Ok(None) => (None, path, path),
        Err(message) => {
            if can_be_url && should_ignore_link_with_disambiguators(path) {
                return None;
            }
            return Some(Err(message));
        }
    };
    let is_shortcut = link.kind == LinkType::ShortcutUnknown;
    let ignore_urllike = can_be_url || (is_shortcut && !link.link.contains('`'));
    if ignore_urllike && should_ignore_link(path) {
        return None;
    }
    if is_shortcut
        && let Some(suffix) = link.link.strip_prefix('!')
        && !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    let path = match strip_generics(path) {
        Ok(path) => path,
        Err(message) => return Some(Err(message.to_owned())),
    };
    if path.contains(' ') {
        return None;
    }
    Some(Ok(Preprocessed {
        path,
        disambiguator,
        fragment: fragment.map(str::to_owned),
        text: text.to_owned(),
    }))
}

fn should_ignore_link_with_disambiguators(link: &str) -> bool {
    link.contains(|ch: char| !(ch.is_alphanumeric() || ":_<>, !*&;@()".contains(ch)))
}

fn should_ignore_link(path: &str) -> bool {
    path.contains(|ch: char| !(ch.is_alphanumeric() || ":_<>, !*&;".contains(ch)))
}

fn strip_generics_segment(segment: &[char]) -> Result<String, &'static str> {
    let mut stripped = String::new();
    let mut depth = 0i32;
    let mut chunk = String::new();
    for &c in segment {
        if c == '<' {
            depth += 1;
            chunk.clear();
        } else if c == '>' {
            depth -= 1;
            if chunk.contains(" as ") {
                return Err("has invalid generics: fully-qualified syntax is unsupported");
            }
        } else if depth == 0 {
            stripped.push(c);
        } else {
            chunk.push(c);
        }
    }
    if depth == 0 { Ok(stripped) } else { Err("has invalid generics: unbalanced angle brackets") }
}

fn strip_generics(path: &str) -> Result<String, &'static str> {
    if !path.contains(['<', '>']) {
        return Ok(path.to_owned());
    }
    let mut segments = vec![];
    let mut chars = path.chars().peekable();
    let mut segment = vec![];
    while let Some(c) = chars.next() {
        match c {
            ':' => {
                if chars.next_if_eq(&':').is_some() {
                    let stripped = strip_generics_segment(&std::mem::take(&mut segment))?;
                    if !stripped.is_empty() {
                        segments.push(stripped);
                    }
                } else {
                    return Err("has invalid generics: invalid path separator");
                }
            }
            '<' => {
                segment.push(c);
                match chars.next() {
                    Some('<') => return Err("has invalid generics: too many angle brackets"),
                    Some('>') => return Err("has invalid generics: empty angle brackets"),
                    Some(c) => {
                        segment.push(c);
                        while let Some(c) = chars.next_if(|c| *c != '>') {
                            segment.push(c);
                        }
                    }
                    None => break,
                }
            }
            _ => segment.push(c),
        }
    }
    if !segment.is_empty() {
        let stripped = strip_generics_segment(&segment)?;
        if !stripped.is_empty() {
            segments.push(stripped);
        }
    }
    if segments.is_empty() {
        Err("has invalid generics: missing type for generic parameters")
    } else {
        Ok(segments.join("::"))
    }
}

/// The primitive type a path names, as rustdoc's `resolve_primitive` has them; the name is the
/// one its page in `std` has.
pub(crate) fn primitive(path: &str) -> Option<&'static str> {
    Some(match path {
        "isize" => "isize",
        "i8" => "i8",
        "i16" => "i16",
        "i32" => "i32",
        "i64" => "i64",
        "i128" => "i128",
        "usize" => "usize",
        "u8" => "u8",
        "u16" => "u16",
        "u32" => "u32",
        "u64" => "u64",
        "u128" => "u128",
        "f16" => "f16",
        "f32" => "f32",
        "f64" => "f64",
        "f128" => "f128",
        "char" => "char",
        "bool" | "true" | "false" => "bool",
        "str" | "&str" => "str",
        "slice" => "slice",
        "array" => "array",
        "tuple" => "tuple",
        "unit" => "unit",
        "pointer" | "*const" | "*mut" => "pointer",
        "reference" | "&" | "&mut" => "reference",
        "fn" => "fn",
        "never" | "!" => "never",
        _ => return None,
    })
}

/// What an intra-doc link resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    Def(ModuleDef),
    Field(hir::Field),
    /// A primitive type, and a method or other item of it.
    Primitive(&'static str, Option<String>),
}

/// Resolves `link` as the docs on `def` have it, where an inner doc comment (`//!`) resolves in
/// the module itself rather than its parent.
pub(crate) fn resolve(
    db: &Db,
    def: impl hir::HasAttrs + Copy,
    link: &Preprocessed,
    is_inner: hir::IsInnerDoc,
) -> Option<Resolved> {
    let path = link.path.as_str();
    if link.disambiguator == Some(Disambiguator::Primitive) {
        return primitive(path).map(|it| Resolved::Primitive(it, None));
    }
    let ns = link.disambiguator.map(Disambiguator::ns);
    let try_ns = |ns: Option<Namespace>| {
        hir::resolve_doc_path_on(db, def, path, ns, is_inner).and_then(|found| match found {
            DocLinkDef::ModuleDef(it) => {
                link.disambiguator.is_none_or(|d| d.accepts(&it)).then_some(Resolved::Def(it))
            }
            DocLinkDef::Field(it) => Some(Resolved::Field(it)),
            DocLinkDef::SelfType(it) => Some(Resolved::Def(ModuleDef::Trait(it))),
        })
    };
    let found = match ns {
        Some(ns) => try_ns(Some(ns)),
        // rustdoc tries the type namespace, then values, then macros.
        None => try_ns(Some(Namespace::Types))
            .or_else(|| try_ns(Some(Namespace::Values)))
            .or_else(|| try_ns(Some(Namespace::Macros)))
            .or_else(|| try_ns(None)),
    };
    if found.is_some() {
        return found;
    }
    if matches!(ns, None | Some(Namespace::Types))
        && let Some(prim) = primitive(path)
    {
        return Some(Resolved::Primitive(prim, None));
    }
    // `str::len`, `u8::MAX`: an item of a primitive type.
    if let Some((base, item)) = path.rsplit_once("::")
        && let Some(prim) = primitive(base)
    {
        return Some(Resolved::Primitive(prim, Some(item.to_owned())));
    }
    None
}
