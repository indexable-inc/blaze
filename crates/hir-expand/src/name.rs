//! See [`Name`].

use std::fmt;

use base_db::Db;
use intern::{Symbol, sym};
use span::{Edition, SyntaxContext};
use syntax::utils::is_raw_identifier;
use syntax::{ast, format_smolstr, view};

/// `Name` is a wrapper around string, which is used in hir for both references
/// and declarations. In theory, names should also carry hygiene info, but we are
/// not there yet!
///
/// Note that the rawness (`r#`) of names is not preserved. Names are always stored without a `r#` prefix.
/// This is because we want to show (in completions etc.) names as raw depending on the needs
/// of the current crate, for example if it is edition 2021 complete `gen` even if the defining
/// crate is in edition 2024 and wrote `r#gen`, and the opposite holds as well.
///
/// 4 bytes, `Copy`: the interned symbol's id. Equality and hashing are one `u32`; ordering
/// compares text (ids differ between runs), equal ids first.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Name {
    symbol: Symbol,
    // If you are making this carry actual hygiene, beware that the special handling for variables and labels
    // in bodies can go.
    ctx: (),
}

const _: () = assert!(size_of::<Name>() == size_of::<Symbol>());
const _: () = assert!(size_of::<Option<Name>>() == size_of::<Symbol>());

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Name")
            .field("symbol", &self.symbol.as_str())
            .field("ctx", &self.ctx)
            .finish()
    }
}

impl Ord for Name {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.symbol.cmp_order(&other.symbol)
    }
}

impl PartialOrd for Name {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// No need to strip `r#`, all comparisons are done against well-known symbols.
impl PartialEq<Symbol> for Name {
    #[inline]
    fn eq(&self, sym: &Symbol) -> bool {
        self.symbol == *sym
    }
}

impl PartialEq<&Symbol> for Name {
    #[inline]
    fn eq(&self, &sym: &&Symbol) -> bool {
        self.symbol == *sym
    }
}

impl PartialEq<Name> for Symbol {
    #[inline]
    fn eq(&self, name: &Name) -> bool {
        *self == name.symbol
    }
}

impl PartialEq<Name> for &Symbol {
    #[inline]
    fn eq(&self, name: &Name) -> bool {
        **self == name.symbol
    }
}

impl Name {
    #[inline]
    fn new_text(text: &str) -> Name {
        Name { symbol: Symbol::intern(text), ctx: () }
    }

    #[inline]
    pub fn new(text: &str, mut ctx: SyntaxContext) -> Name {
        // For comparisons etc. we remove the edition, because sometimes we search for some `Name`
        // and we don't know which edition it came from.
        // Can't do that for all `SyntaxContextId`s because it breaks the query engine.
        ctx.remove_root_edition();
        _ = ctx;
        let text = text.strip_prefix("r#").unwrap_or(text);
        Self::new_text(text)
    }

    #[inline]
    pub fn new_root(text: &str) -> Name {
        // The edition doesn't matter for hygiene.
        Self::new(text, SyntaxContext::root(Edition::Edition2015))
    }

    #[inline]
    pub fn new_tuple_field(idx: usize) -> Name {
        Name::new_symbol_root(sym::Integer::get(idx))
    }

    #[inline]
    pub fn new_lifetime(lt: &str) -> Name {
        match lt.strip_prefix("'r#") {
            Some(lt) => Self::new_text(&format_smolstr!("'{lt}")),
            None => Self::new_text(lt),
        }
    }

    #[inline]
    pub fn new_symbol(symbol: Symbol, ctx: SyntaxContext) -> Self {
        debug_assert!(!symbol.as_str().starts_with("r#"));
        _ = ctx;
        Self { symbol, ctx: () }
    }

    // FIXME: This needs to go once we have hygiene
    #[inline]
    pub fn new_symbol_root(sym: Symbol) -> Self {
        Self::new_symbol(sym, SyntaxContext::root(Edition::Edition2015))
    }

    /// A fake name for things missing in the source code.
    ///
    /// For example, `impl Foo for {}` should be treated as a trait impl for a
    /// type with a missing name. Similarly, `struct S { : u32 }` should have a
    /// single field with a missing name.
    ///
    /// Ideally, we want a `gensym` semantics for missing names -- each missing
    /// name is equal only to itself. It's not clear how to implement this in
    /// the query engine though, so we punt on that bit for a moment.
    #[inline]
    pub const fn missing() -> Name {
        Name { symbol: sym::MISSING_NAME, ctx: () }
    }

    /// Returns true if this is a fake name for things missing in the source code. See
    /// [`missing()`][Self::missing] for details.
    ///
    /// Use this method instead of comparing with `Self::missing()` as missing names
    /// (ideally should) have a `gensym` semantics.
    #[inline]
    pub fn is_missing(&self) -> bool {
        self.symbol == sym::MISSING_NAME
    }

    /// Generates a new name that attempts to be unique. Should only be used when body lowering and
    /// creating desugared locals and labels. The caller is responsible for picking an index
    /// that is stable across re-executions
    #[inline]
    pub fn generate_new_name(idx: usize) -> Name {
        Name::new_symbol_root(sym::RaGeneratedName::get(idx))
    }

    /// Returns the tuple index this name represents if it is a tuple field.
    #[inline]
    pub fn as_tuple_index(&self) -> Option<usize> {
        sym::Integer::as_uint(&self.symbol)
    }

    /// Whether this name needs to be escaped in the given edition via `r#`.
    #[inline]
    pub fn needs_escape(&self, edition: Edition) -> bool {
        is_raw_identifier(self.symbol.as_str(), edition)
    }

    /// Returns the text this name represents if it isn't a tuple field.
    ///
    /// Do not use this for user-facing text, use `display` instead to handle editions properly.
    // FIXME: This should take a database argument to hide the interning
    #[inline]
    pub fn as_str(&self) -> &str {
        self.symbol.as_str()
    }

    #[inline]
    pub fn display<'a>(
        &'a self,
        db: &Db,
        edition: Edition,
    ) -> impl fmt::Display + 'a {
        _ = db;
        self.display_no_db(edition)
    }

    // FIXME: Remove this in favor of `display`, see fixme on `as_str`
    #[doc(hidden)]
    #[inline]
    pub fn display_no_db(&self, edition: Edition) -> impl fmt::Display + '_ {
        Display { name: self, edition }
    }

    #[inline]
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    #[inline]
    pub fn is_generated(&self) -> bool {
        is_generated(self.as_str())
    }
}

#[inline]
pub fn is_generated(name: &str) -> bool {
    name.starts_with("<ra@gennew>")
}

struct Display<'a> {
    name: &'a Name,
    edition: Edition,
}

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut symbol = self.name.symbol.as_str();

        if symbol == "'static" {
            // FIXME: '`static` can also be a label, and there it does need escaping.
            // But knowing where it is will require adding a parameter to `display()`,
            // and that is an infectious change.
            return f.write_str(symbol);
        }

        if let Some(s) = symbol.strip_prefix('\'') {
            f.write_str("'")?;
            symbol = s;
        }
        if is_raw_identifier(symbol, self.edition) {
            f.write_str("r#")?;
        }
        f.write_str(symbol)
    }
}

pub trait AsName {
    fn as_name(&self) -> Name;
}

impl AsName for ast::NameRef {
    fn as_name(&self) -> Name {
        let text = self.text();
        if text.as_bytes().first().is_some_and(u8::is_ascii_digit)
            && let Ok(idx) = text.parse::<usize>()
        {
            return Name::new_tuple_field(idx);
        }
        Name::new_root(text)
    }
}

impl AsName for ast::Name {
    fn as_name(&self) -> Name {
        Name::new_root(self.text())
    }
}

impl AsName for ast::NameOrNameRef {
    fn as_name(&self) -> Name {
        match self {
            ast::NameOrNameRef::Name(it) => it.as_name(),
            ast::NameOrNameRef::NameRef(it) => it.as_name(),
        }
    }
}

impl AsName for tt::Ident {
    fn as_name(&self) -> Name {
        // A token tree keeps rawness in `is_raw`, so its symbol is already the name's.
        if self.sym.as_str().starts_with("r#") {
            return Name::new_root(self.sym.as_str());
        }
        Name::new_symbol_root(self.sym)
    }
}

impl AsName for ast::FieldKind {
    fn as_name(&self) -> Name {
        match self {
            ast::FieldKind::Name(nr) => nr.as_name(),
            ast::FieldKind::Index(idx) => tuple_index_name(idx.text()),
        }
    }
}

impl AsName for view::NameRef<'_> {
    fn as_name(&self) -> Name {
        let text = self.text();
        if text.as_bytes().first().is_some_and(u8::is_ascii_digit)
            && let Ok(idx) = text.parse::<usize>()
        {
            return Name::new_tuple_field(idx);
        }
        Name::new_root(text)
    }
}

impl AsName for view::Name<'_> {
    fn as_name(&self) -> Name {
        Name::new_root(self.text())
    }
}

impl AsName for view::NameOrNameRef<'_> {
    fn as_name(&self) -> Name {
        match self {
            view::NameOrNameRef::Name(it) => it.as_name(),
            view::NameOrNameRef::NameRef(it) => it.as_name(),
        }
    }
}

impl AsName for view::FieldKind<'_> {
    fn as_name(&self) -> Name {
        match self {
            view::FieldKind::Name(nr) => nr.as_name(),
            view::FieldKind::Index(idx) => tuple_index_name(idx.text()),
        }
    }
}

/// The name a field access `x.<text>` names: rustc's field ident is the literal's own text, so a
/// text that is not a plain decimal `usize` (`0x1`, `1_0`, `01`, one past `usize::MAX`) names no
/// tuple field, and the access fails as rustc's E0609 does.
fn tuple_index_name(text: &str) -> Name {
    match text.parse::<usize>() {
        Ok(idx) if !(text.len() > 1 && text.starts_with('0')) => Name::new_tuple_field(idx),
        _ => Name::new_root(text),
    }
}

impl AsName for base_db::BuiltDependency {
    fn as_name(&self) -> Name {
        Name::new_symbol_root(*self.name)
    }
}

#[cfg(test)]
mod tests {
    use span::{EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor};
    use syntax::{SourceFile, TextRange, TextSize, view::AstNode as _};

    use super::*;

    const WORDS: &[&str] =
        &["a", "b", "Self", "self", "zeta", "Alpha", "_x", "0", "10", "9", "'a", "gen", "ü"];

    #[test]
    fn order_is_by_text() {
        let mut names: Vec<Name> = WORDS.iter().map(|it| Name::new_root(it)).collect();
        let mut texts: Vec<&str> = WORDS.to_vec();
        names.sort();
        texts.sort();
        assert_eq!(names.iter().map(Name::as_str).collect::<Vec<_>>(), texts);
        for (a, b) in names.iter().zip(&texts) {
            assert_eq!(a.cmp(&Name::new_root(b)), std::cmp::Ordering::Equal);
        }
    }

    #[test]
    fn idents_name_their_symbol() {
        let span = Span {
            range: TextRange::empty(TextSize::new(0)),
            anchor: SpanAnchor {
                file_id: EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT),
                ast_id: ROOT_ERASED_FILE_AST_ID,
            },
            ctx: SyntaxContext::root(Edition::CURRENT),
        };
        for word in WORDS.iter().chain(&["r#gen", "r#fn"]) {
            let ident = tt::Ident { sym: Symbol::intern(word), span, is_raw: tt::IdentIsRaw::No };
            assert_eq!(ident.as_name(), Name::new_root(word), "{word}");
        }
    }

    #[test]
    fn name_refs_and_tuple_fields() {
        let file = SourceFile::parse(
            "fn f() { x.0; x.17; x.abc; x.r#gen; x._1; x.a0; }",
            Edition::CURRENT,
        );
        let names: Vec<Name> = file
            .root()
            .descendants()
            .filter_map(view::NameRef::cast)
            .filter(|it| it.text() != "x")
            .map(|it| it.as_name())
            .collect();
        let old: Vec<Name> = file
            .root()
            .descendants()
            .filter_map(view::NameRef::cast)
            .filter(|it| it.text() != "x")
            .map(|it| match it.as_tuple_field() {
                Some(idx) => Name::new_tuple_field(idx),
                None => Name::new_root(it.text()),
            })
            .collect();
        assert_eq!(names, old);
        assert_eq!(names[0].as_tuple_index(), Some(0));
        assert_eq!(names[1].as_tuple_index(), Some(17));
        assert_eq!(names[2].as_str(), "abc");
        assert_eq!(names[3].as_str(), "gen");
    }

    /// A field index that is not plain decimal names its own text, never field 0.
    #[test]
    fn tuple_index_names_its_text() {
        assert_eq!(tuple_index_name("0").as_tuple_index(), Some(0));
        assert_eq!(tuple_index_name("17").as_tuple_index(), Some(17));
        for text in ["0x1", "01", "1_0", "99999999999999999999999999"] {
            let name = tuple_index_name(text);
            assert_eq!(name.as_str(), text);
            assert_ne!(name, Name::new_tuple_field(0), "{text}");
            assert_ne!(name, Name::new_tuple_field(1), "{text}");
        }
    }
}
