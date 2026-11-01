//! What the documentation says, apart from how a page shows it: the crates documented, their
//! items where rustdoc puts them, each signature as code pieces whose names link, the docs
//! with their intra-doc links resolved, and the impls of every type and trait.

use blaze_hash::HashMap;

/// Every crate documented in one run.
#[derive(Debug, Default)]
pub struct Model {
    pub crates: Vec<CrateDoc>,
    pub warnings: Vec<Warning>,
}

impl Model {
    pub fn krate(&self, name: &str) -> Option<&CrateDoc> {
        self.crates.iter().find(|it| it.name == name)
    }
}

/// A warning rustdoc gives too, said of a crate the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub krate: String,
    pub message: String,
    /// Where, as `path:line:column` from the workspace root, when the source is known.
    pub at: Option<String>,
}

#[derive(Debug)]
pub struct CrateDoc {
    /// The name code uses (`serde_json`), which is the directory under `doc/`.
    pub name: String,
    pub version: Option<String>,
    /// The root module is `items[0]`.
    pub items: Vec<Item>,
    pub sources: Vec<SourceFile>,
    /// Whether items not public are documented too, as `--document-private-items` and a binary
    /// have it.
    pub private: bool,
}

impl CrateDoc {
    pub fn root(&self) -> &Item {
        &self.items[0]
    }

    /// The item at `path` (module path then name), as a test asks for it.
    pub fn find(&self, path: &str) -> Option<&Item> {
        self.items.iter().find(|it| it.full_path() == path)
    }
}

/// A source file shown as a page under `src/<crate>/`.
#[derive(Debug)]
pub struct SourceFile {
    /// Relative to the crate root's directory, with `/` between parts; a file outside it (one
    /// a `#[path]` or an `include!` names) is under `-/`.
    pub path: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Module,
    Struct,
    Enum,
    Union,
    Trait,
    TraitAlias,
    Function,
    TypeAlias,
    Constant,
    Static,
    Macro,
    ProcAttribute,
    ProcDerive,
    Primitive,
    Keyword,
    // Members, documented on their parent's page.
    Method,
    TyMethod,
    StructField,
    Variant,
    AssocType,
    AssocConst,
}

impl Kind {
    /// The word rustdoc uses for the kind in file names, CSS classes and the sidebar lists.
    pub fn short(self) -> &'static str {
        match self {
            Kind::Module => "mod",
            Kind::Struct => "struct",
            Kind::Enum => "enum",
            Kind::Union => "union",
            Kind::Trait => "trait",
            Kind::TraitAlias => "traitalias",
            Kind::Function => "fn",
            Kind::TypeAlias => "type",
            Kind::Constant => "constant",
            Kind::Static => "static",
            Kind::Macro => "macro",
            Kind::ProcAttribute => "attr",
            Kind::ProcDerive => "derive",
            Kind::Primitive => "primitive",
            Kind::Keyword => "keyword",
            Kind::Method => "method",
            Kind::TyMethod => "tymethod",
            Kind::StructField => "structfield",
            Kind::Variant => "variant",
            Kind::AssocType => "associatedtype",
            Kind::AssocConst => "associatedconstant",
        }
    }

    /// The heading word: "Struct", "Function".
    pub fn title(self) -> &'static str {
        match self {
            Kind::Module => "Module",
            Kind::Struct => "Struct",
            Kind::Enum => "Enum",
            Kind::Union => "Union",
            Kind::Trait => "Trait",
            Kind::TraitAlias => "Trait Alias",
            Kind::Function => "Function",
            Kind::TypeAlias => "Type Alias",
            Kind::Constant => "Constant",
            Kind::Static => "Static",
            Kind::Macro => "Macro",
            Kind::ProcAttribute => "Attribute Macro",
            Kind::ProcDerive => "Derive Macro",
            Kind::Primitive => "Primitive Type",
            Kind::Keyword => "Keyword",
            Kind::Method => "Method",
            Kind::TyMethod => "Required Method",
            Kind::StructField => "Field",
            Kind::Variant => "Variant",
            Kind::AssocType => "Associated Type",
            Kind::AssocConst => "Associated Constant",
        }
    }

    /// The section a module page lists the kind under.
    pub fn plural(self) -> &'static str {
        match self {
            Kind::Module => "Modules",
            Kind::Struct => "Structs",
            Kind::Enum => "Enums",
            Kind::Union => "Unions",
            Kind::Trait => "Traits",
            Kind::TraitAlias => "Trait Aliases",
            Kind::Function => "Functions",
            Kind::TypeAlias => "Type Aliases",
            Kind::Constant => "Constants",
            Kind::Static => "Statics",
            Kind::Macro => "Macros",
            Kind::ProcAttribute => "Attribute Macros",
            Kind::ProcDerive => "Derive Macros",
            Kind::Primitive => "Primitive Types",
            Kind::Keyword => "Keywords",
            Kind::Method => "Methods",
            Kind::TyMethod => "Required Methods",
            Kind::StructField => "Fields",
            Kind::Variant => "Variants",
            Kind::AssocType => "Associated Types",
            Kind::AssocConst => "Associated Constants",
        }
    }

    /// The anchor of the section on a module page: rustdoc's ids.
    pub fn section_id(self) -> &'static str {
        match self {
            Kind::Module => "modules",
            Kind::Struct => "structs",
            Kind::Enum => "enums",
            Kind::Union => "unions",
            Kind::Trait => "traits",
            Kind::TraitAlias => "trait-aliases",
            Kind::Function => "functions",
            Kind::TypeAlias => "types",
            Kind::Constant => "constants",
            Kind::Static => "statics",
            Kind::Macro => "macros",
            Kind::ProcAttribute => "attributes",
            Kind::ProcDerive => "derives",
            Kind::Primitive => "primitives",
            Kind::Keyword => "keywords",
            Kind::Method => "implementations",
            Kind::TyMethod => "required-methods",
            Kind::StructField => "fields",
            Kind::Variant => "variants",
            Kind::AssocType => "required-associated-types",
            Kind::AssocConst => "required-associated-consts",
        }
    }

    /// Module members in the order rustdoc's module pages list them.
    pub const MODULE_ORDER: [Kind; 15] = [
        Kind::Keyword,
        Kind::Primitive,
        Kind::Module,
        Kind::Macro,
        Kind::Struct,
        Kind::Enum,
        Kind::Constant,
        Kind::Static,
        Kind::Trait,
        Kind::Function,
        Kind::TypeAlias,
        Kind::Union,
        Kind::ProcAttribute,
        Kind::ProcDerive,
        Kind::TraitAlias,
    ];

    pub fn has_page(self) -> bool {
        !matches!(
            self,
            Kind::Method
                | Kind::TyMethod
                | Kind::StructField
                | Kind::Variant
                | Kind::AssocType
                | Kind::AssocConst
        )
    }
}

/// Where an item's documentation is: a page in some crate's docs, and an anchor on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemRef {
    pub krate: String,
    /// Modules from the crate root down to the one the item's page is in (empty at the root).
    pub modules: Vec<String>,
    pub kind: Kind,
    pub name: String,
    /// A member: its anchor on the page, as `method.len`.
    pub anchor: Option<String>,
    /// A member: what rustdoc's link titles call it, `method` or `associated function`.
    pub descr: Option<&'static str>,
    /// The path a link's title names when it is not the page's: where an item of another crate
    /// is declared, as rustdoc names what the crate linking did not inline.
    pub declared: Option<String>,
    /// An item of a crate not documented here that says where its docs are
    /// (`#![doc(html_root_url)]`): that URL, ending in `/`, which `page` is under.
    pub remote: Option<String>,
}

impl ItemRef {
    /// The page, relative to the documentation root: `serde/de/trait.Deserialize.html`, a
    /// module's `index.html` in its own directory.
    pub fn page(&self) -> String {
        let mut path = self.krate.clone();
        for module in &self.modules {
            path.push('/');
            path.push_str(module);
        }
        if self.kind == Kind::Module {
            if !self.name.is_empty() {
                path.push('/');
                path.push_str(&self.name);
            }
            path.push_str("/index.html");
        } else {
            path.push('/');
            path.push_str(self.kind.short());
            path.push('.');
            path.push_str(&self.name);
            path.push_str(".html");
        }
        path
    }

    /// The path a link's title names: `serde::de::Deserialize`.
    pub fn display_path(&self) -> String {
        if let Some(declared) = &self.declared {
            return declared.clone();
        }
        let mut parts = vec![self.krate.as_str()];
        parts.extend(self.modules.iter().map(String::as_str));
        if !self.name.is_empty() {
            parts.push(&self.name);
        }
        parts.join("::")
    }
}

/// What a name in a signature or an intra-doc link points at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// An item; a primitive type is one of `std`'s (`primitive.u8.html`).
    Item(ItemRef),
    /// A link the docs write out themselves (`[text](https://..)`).
    Url(String),
}

/// A signature or declaration as code: text, with the names in it that point somewhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Code(pub Vec<Piece>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Keyword(String),
    /// The item's own name where its declaration says it.
    Name(String, Kind),
    /// A name that points at its documentation, with the kind it is.
    Link {
        text: String,
        kind: Kind,
        to: Target,
    },
    /// A name with no documentation to point at (a generic parameter, an item of a crate not
    /// documented).
    Plain(String),
    Lifetime(String),
    Attribute(String),
    Comment(String),
}

impl Piece {
    /// The piece's text, as [`Code::plain`] reads it.
    fn plain(&self) -> &str {
        match self {
            Piece::Text(it)
            | Piece::Keyword(it)
            | Piece::Name(it, _)
            | Piece::Plain(it)
            | Piece::Lifetime(it)
            | Piece::Attribute(it)
            | Piece::Comment(it) => it,
            Piece::Link { text, .. } => text,
        }
    }
}

impl Code {
    pub fn text(&mut self, text: impl AsRef<str>) -> &mut Self {
        let text = text.as_ref();
        if text.is_empty() {
            return self;
        }
        if let Some(Piece::Text(last)) = self.0.last_mut() {
            last.push_str(text);
        } else {
            self.0.push(Piece::Text(text.to_owned()));
        }
        self
    }

    pub fn kw(&mut self, kw: &str) -> &mut Self {
        self.0.push(Piece::Keyword(kw.to_owned()));
        self
    }

    pub fn push(&mut self, piece: Piece) -> &mut Self {
        self.0.push(piece);
        self
    }

    pub fn extend(&mut self, other: Code) -> &mut Self {
        for piece in other.0 {
            match piece {
                Piece::Text(text) => {
                    self.text(text);
                }
                it => self.0.push(it),
            }
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The code as plain text, as a test or the search index reads it.
    pub fn plain(&self) -> String {
        let mut out = String::new();
        for piece in &self.0 {
            out.push_str(piece.plain());
        }
        out
    }

    /// `self.plain().cmp(&other.plain())`, with nothing allocated.
    pub fn cmp_plain(&self, other: &Code) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let (mut a, mut b) = (self.0.iter(), other.0.iter());
        let (mut x, mut y): (&[u8], &[u8]) = (&[], &[]);
        loop {
            while x.is_empty() {
                let Some(it) = a.next() else { break };
                x = it.plain().as_bytes();
            }
            while y.is_empty() {
                let Some(it) = b.next() else { break };
                y = it.plain().as_bytes();
            }
            match (x.is_empty(), y.is_empty()) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                (false, false) => {}
            }
            let n = x.len().min(y.len());
            match x[..n].cmp(&y[..n]) {
                Ordering::Equal => (x, y) = (&x[n..], &y[n..]),
                unequal => return unequal,
            }
        }
    }

    /// Every target the code links to, as tests check them.
    pub fn links(&self) -> impl Iterator<Item = (&str, &Target)> {
        self.0.iter().filter_map(|piece| match piece {
            Piece::Link { text, to, .. } => Some((text.as_str(), to)),
            _ => None,
        })
    }
}

/// Docs as written (Markdown) with what each intra-doc link in them resolved to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub markdown: String,
    /// Keyed by the link's destination as the docs write it (`Foo::bar`, `crate::Baz`), or its
    /// text for a shortcut link (`[Foo]`).
    pub links: HashMap<String, Target>,
    /// What a link whose text is the link itself shows instead: `[struct@Foo]` shows `Foo`.
    pub texts: HashMap<String, String>,
}

impl Doc {
    pub fn is_empty(&self) -> bool {
        self.markdown.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deprecation {
    pub since: Option<String>,
    pub note: Option<String>,
}

/// Where an item is in the sources: the crate documented (its index in [`Model::crates`]), the
/// file's index in its [`CrateDoc::sources`] and the lines, counted from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub krate: usize,
    pub file: usize,
    pub lines: (u32, u32),
}

/// What an item's page or entry shows besides its members.
#[derive(Debug, Clone, Default)]
pub struct Notes {
    pub deprecated: Option<Deprecation>,
    pub must_use: Option<String>,
    /// `#[doc(cfg(..))]` or the item's own `#[cfg(..)]`, as "Available on ... only".
    pub cfg: Option<String>,
    pub is_unsafe: bool,
}

#[derive(Debug)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    /// Modules from the crate root down to the one the item is listed in.
    pub modules: Vec<String>,
    pub docs: Doc,
    /// The declaration block at the top of the page.
    pub decl: Code,
    pub notes: Notes,
    pub source: Option<Span>,
    /// A module's items, as indices into [`CrateDoc::items`].
    pub children: Vec<usize>,
    /// A module's `pub use` lines that rustdoc does not inline.
    pub reexports: Vec<Reexport>,
    /// A struct or union's fields, or those of the one a type alias stands for.
    pub fields: Vec<Member>,
    /// An enum's variants, or those of the one a type alias stands for.
    pub variants: Vec<Member>,
    /// A type alias of a struct, union or enum: that type's declaration under the alias's name.
    pub aliased: Option<Code>,
    /// A trait's own items, in declaration order.
    pub trait_items: Vec<Member>,
    pub impls: Impls,
    /// A struct or union whose fields are not all shown.
    pub hidden_fields: bool,
    /// A trait that is dyn compatible, as its page says.
    pub dyn_compatible: Option<bool>,
}

impl Item {
    pub fn full_path(&self) -> String {
        let mut parts: Vec<&str> = self.modules.iter().map(String::as_str).collect();
        if self.kind != Kind::Module || !self.name.is_empty() {
            parts.push(&self.name);
        }
        parts.join("::")
    }

    pub fn summary_source(&self) -> &str {
        first_paragraph(&self.docs.markdown)
    }
}

/// The Markdown of the first paragraph, which rustdoc shows as the item's summary.
pub fn first_paragraph(markdown: &str) -> &str {
    let trimmed = markdown.trim_start();
    let mut end = trimmed.len();
    let mut at = 0;
    for line in trimmed.split_inclusive('\n') {
        if at > 0 && line.trim().is_empty() {
            end = at;
            break;
        }
        at += line.len();
    }
    trimmed[..end].trim_end()
}

/// A field, a variant, a method or other associated item.
#[derive(Debug, Clone)]
pub struct Member {
    pub kind: Kind,
    pub name: String,
    pub decl: Code,
    pub docs: Doc,
    pub notes: Notes,
    pub source: Option<Span>,
    /// A variant's fields.
    pub fields: Vec<Member>,
    /// An impl's item that has no docs of its own and shows the trait's.
    pub docs_from_trait: bool,
    /// A trait item with a default the trait gives (a provided method).
    pub provided: bool,
}

#[derive(Debug, Clone)]
pub struct Reexport {
    /// `pub use foo::Bar;` as code, the path linking where it can.
    pub decl: Code,
    pub name: String,
    pub docs: Doc,
}

#[derive(Debug, Default)]
pub struct Impls {
    pub inherent: Vec<ImplDoc>,
    pub traits: Vec<ImplDoc>,
    pub auto: Vec<ImplDoc>,
    pub blanket: Vec<ImplDoc>,
    /// On a trait's page: the impls of it, from every crate documented.
    pub implementors: Vec<ImplDoc>,
    /// On a trait's page: the impls on types from elsewhere, with their items, rustdoc's
    /// "Implementations on Foreign Types".
    pub foreign: Vec<ImplDoc>,
    /// On a trait's page: the impls rustdoc lists as "Implementors" too, blanket ones whose
    /// self type is a parameter.
    pub blanket_implementors: Vec<ImplDoc>,
    /// The methods `Deref` impls bring, the target's and then its own target's.
    pub deref: Vec<DerefMethods>,
}

/// rustdoc's "Methods from Deref<Target = [u8]>".
#[derive(Debug, Clone)]
pub struct DerefMethods {
    /// `Deref`, linked.
    pub trait_: Code,
    pub target: Code,
    /// `deref-methods-%5Bu8%5D`.
    pub anchor: String,
    pub members: Vec<Member>,
}

impl Impls {
    pub fn is_empty(&self) -> bool {
        self.inherent.is_empty()
            && self.traits.is_empty()
            && self.auto.is_empty()
            && self.blanket.is_empty()
            && self.implementors.is_empty()
            && self.foreign.is_empty()
            && self.blanket_implementors.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ImplDoc {
    /// `impl<T: Clone> Clone for Foo<T>`, every name linking.
    pub header: Code,
    /// The anchor rustdoc gives the impl: `impl-Clone-for-Foo%3CT%3E`.
    pub anchor: String,
    /// The trait's name, for the sidebar.
    pub trait_name: Option<String>,
    pub items: Vec<Member>,
    pub docs: Doc,
    pub source: Option<Span>,
    pub negative: bool,
    /// An auto trait impl rustdoc works out rather than one written.
    pub synthetic: bool,
}

#[cfg(test)]
mod tests {
    use super::{Code, Piece};

    #[test]
    fn cmp_plain_is_the_order_of_plain() {
        let code = |pieces: &[&str]| {
            Code(pieces.iter().enumerate().map(|(at, it)| {
                if at % 2 == 0 { Piece::Text((*it).to_owned()) } else { Piece::Keyword((*it).to_owned()) }
            }).collect())
        };
        let all = [
            code(&[]),
            code(&[""]),
            code(&["impl"]),
            code(&["im", "pl"]),
            code(&["impl", " Clone"]),
            code(&["impl ", "Clone", " for"]),
            code(&["impl", " Copy"]),
            code(&["", "impl", "", " C"]),
            code(&["impl Clon"]),
            code(&["z"]),
        ];
        for a in &all {
            for b in &all {
                assert_eq!(a.cmp_plain(b), a.plain().cmp(&b.plain()), "{a:?} against {b:?}");
            }
        }
    }
}
